# Launchcraft

<div align="center">

[![CI](https://github.com/cmspeedau/Launchcraft/actions/workflows/ci.yml/badge.svg)](https://github.com/cmspeedau/Launchcraft/actions/workflows/ci.yml)
[![Docker](https://img.shields.io/badge/docker-container-blue?logo=docker&logoColor=white)](Dockerfile)
[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.85%2B-orange.svg?logo=rust&logoColor=white)](Cargo.toml)
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
  - **One server, many Java majors:** runtimes are pooled per major
    (`<data>/jres/java-<n>`), so a `1.7.10` pack (Java 8) and a `1.20.5` pack
    (Java 21) coexist; explicit paths (`LAUNCHCRAFT_JAVA_<n>`), a host JDK
    (PATH / `JAVA_HOME`) and Temurin downloads are consulted in that order.
- **Secure Microsoft & Xbox Authentication:**
  - OAuth 2.0 Device Code and Loopback flow.
  - Xbox Live and XSTS authentication token exchange with secure local credential rotation.
- **Full Minecraft Install & Launch Pipeline:**
  - Mojang version manifest, profile inheritance (`inheritsFrom`), sha1-verified
    client jar / asset / library downloads into a shared `versions/`, `assets/`,
    `libraries/` store — plus Fabric and Quilt overlays, all from the egui client.
  - PLAY builds the real launch command (rule-filtered classpath, natives,
    substituted `${auth_*}` arguments) and streams the game log.
  - Or skip the download entirely: a launchcraft-server can stage the whole
    runtime, and installing a pack pulls `versions/`, `assets/` and
    `libraries/` from it over the shared CAS instead of Mojang (additive,
    hash-verified, nothing pruned).
- **Headless Pack Builder CLI (`launchcraft-builder`):**
  - Standalone CLI for pack creators and server operators.
  - Scans modpack source trees, computes CAS blobs, and emits synchronized `manifest.json` distributions ready for S3 / CDN hosting.
- **Dockerized CI/CD Toolchain:**
  - Ready-to-use container for automated modpack building in GitHub Actions or GitLab CI pipelines.
- **Server Console (`launchcraft-server`):**
  - Serves the backup endpoints the clients POST to, with BLAKE3 checksum storage,
    server-side upload policy enforcement, and optional bearer-token auth.
  - Embedded admin GUI (browser-based): dashboard, archive browser with download/delete,
    upload-policy toggle, and a SKCraft-inspired **Pack Manager** — a pack registry with
    New Pack (scaffold a source workspace), Add Existing, Modify, in-process Build
    (runs the pack builder), and Check (verifies every CAS blob by BLAKE3).
  - Multi-provider **modpack browser** (CurseForge · Modrinth · FTB) and archive upload,
    importing straight into a buildable workspace with loader data fetched.
  - **Server-side runtime distribution:** stages the Minecraft runtime each pack needs
    (profiles, client jar, asset index, libraries) into one shared, deduplicated CAS,
    served to clients over `/runtimes` with refcounted garbage collection.

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
│   ├── launchcraft-builder/     # Headless CLI for scanning and building CAS modpacks
│   └── launchcraft-server/      # Backup API + embedded admin console for server ops
├── compose.yaml                 # Builder + local pack repository (port 8080) stack
├── docker/                      # repo-server static hosting config
├── Dockerfile                   # Multi-stage container for headless pack compilation
└── ROADMAP.md                   # Detailed architecture and milestone tracking
```

---

## Docker & Container Environment

The headless pack builder (`launchcraft-builder`) is packaged as a minimal, secure multi-stage Docker image based on Debian slim, and Compose files wire up the builder plus a local pack repository server.

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
| `REPO_PORT` | Environment | `8080` | Host port on which `repo-server` publishes the built pack over HTTP |

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
    --pack-id "my-server-pack" \
    --pack-version "1.0.0"
```

Or use the bundled Compose stack (falls back to a local image build when
the published image is absent):

```bash
docker compose run --rm builder init --path /workspace/pack/pack.json
docker compose run --rm builder        # ./pack -> ./dist
```

### Serving a Pack Repository

The `repo-server` service publishes the built `./dist` directory as a static
HTTP pack repository on **port 8080** (override with `REPO_PORT`):

```bash
docker compose up -d repo-server

# Repository endpoints
curl http://localhost:8080/manifest.json
curl http://localhost:8080/cas/objects/<xx>/<blake3-hash>
```

Launchers point their repository configuration at `http://localhost:8080`.
The sync engine fetches exactly this layout: the path-keyed `manifest.json`
at the root and zstd-encoded blobs at `/cas/objects/<xx>/<blake3-hash>`, so
the builder → server → client round-trip is covered end-to-end by the
workspace test suite. The same layout is ready for S3, nginx, or any static
CDN — no path rewriting required.

### Running the Server Console (Backups)

`launchcraft-server` is the server-side counterpart of the backup engine:
it accepts the world ZIP uploads clients push from their *World Saves* tab
and serves an admin console for the operator.

```bash
# Native
cargo run -p launchcraft-server
# Or with Compose (persisted in ./server-data, console on port 9090)
docker compose up -d backup-server
```

| Variable | Default | Description |
| :--- | :--- | :--- |
| `LAUNCHCRAFT_ADDR` | `0.0.0.0:9090` | Listen address |
| `LAUNCHCRAFT_DATA_DIR` | `./launchcraft-server-data` | Stores `backups/` + `policy.json` |
| `LAUNCHCRAFT_BACKUP_TOKEN` | *(unset)* | When set, every data endpoint requires `Authorization: Bearer` |
| `LAUNCHCRAFT_MAX_UPLOAD_MB` | `256` | Per-upload size cap |
| `LAUNCHCRAFT_REPO_DIST` | *(unset)* | Built `./dist` dir auto-registered into the Pack Manager on first boot |
| `LAUNCHCRAFT_CURSEFORGE_KEY` | *(unset)* | CurseForge Core API key; enables browsing CurseForge in the console |
| `LAUNCHCRAFT_CURSEFORGE_URL` | `https://api.curseforge.com/v1` | CurseForge API root (override for mirrors/tests) |
| `LAUNCHCRAFT_MODRINTH_URL` | `https://api.modrinth.com/v2` | Modrinth API root (keyless provider; override for mirrors/tests) |
| `LAUNCHCRAFT_FTB_URL` | `https://dist.creeper.host/FTB2` | FTB legacy pack CDN root (keyless provider) |
| `LAUNCHCRAFT_FABRIC_URL` | `https://meta.fabricmc.net/v2` | Fabric loader meta API (loader data fetch) |
| `LAUNCHCRAFT_QUILT_URL` | `https://meta.quiltmc.org/v3` | Quilt loader meta API (loader data fetch) |
| `LAUNCHCRAFT_FORGE_URL` | `https://maven.minecraftforge.net` | Forge Maven (installer jar fetch) |
| `LAUNCHCRAFT_NEOFORGE_URL` | `https://maven.neoforged.net/releases` | NeoForge Maven (installer jar fetch) |
| `LAUNCHCRAFT_META_URL` | `https://piston-meta.mojang.com` | Mojang metadata root (server-side runtime builds) |
| `LAUNCHCRAFT_RESOURCES_URL` | `https://resources.download.minecraft.net` | Mojang asset-object CDN (runtime builds) |
| `LAUNCHCRAFT_RUNTIME_ASSETS` | *(off)* | `1`/`true`/`full` also stages every asset object, not just the index |
| `LAUNCHCRAFT_JRES` | `<data_dir>/jres` | Where managed Java runtimes are provisioned |
| `LAUNCHCRAFT_ADOPTIUM_URL` | `https://api.adoptium.net/v3` | Adoptium API root (mirror it for air-gapped hosts) |
| `LAUNCHCRAFT_JAVA` | *(unset)* | A fallback `java` binary; used only when it really is the needed major |
| `LAUNCHCRAFT_JAVA_<N>` | *(unset)* | Exact path to a Java `<N>` JDK (e.g. `LAUNCHCRAFT_JAVA_8`), wins over downloads |
| `LAUNCHCRAFT_JRES_HOST_JAVA` | `1` | Set `0` to ignore the host's own JDKs (`PATH` / `JAVA_HOME`) and keep the pool strictly managed |

The launcher needs **one** URL for both jobs: enter `http://localhost:9090`
as the **Server URL** (Home tab) and world backups go to
`http://localhost:9090/api` automatically (the client contract is
`POST /api/backups/upload?filename=…` /
`GET /api/backups/download?filename=…`). Open
`http://localhost:9090` for the console. Flipping the console's policy
toggle makes the API answer `403` to uploads, which clients surface as
"disabled by server policy".

### Serving Packs to Launchers

The same server also distributes built packs — publicly, exactly like a
static nginx repo-server (the admin token never gates these routes):

| Route | Purpose |
| :--- | :--- |
| `GET /packs` | Installable pack list shown by launcher clients |
| `GET /packs/<id>/manifest.json` | `SyncEngine` manifest fetch |
| `GET /packs/<id>/cas/objects/<shard>/<hash>` | CAS blobs |

Each pack row also carries its `runtime_id`, and the same server distributes the
Minecraft runtimes themselves:

| Route | Purpose |
| :--- | :--- |
| `GET /runtimes` | Built runtimes (vanilla / Fabric / Quilt) with size and file count |
| `GET /runtimes/<id>/manifest.json` | Runtime manifest in the same format as a pack |
| `GET /runtimes/<id>/cas/objects/<shard>/<hash>` | Runtime blobs (shared store) |

### Server-Side Runtime Distribution

Importing a pack queues a build of the Minecraft runtime it needs — version
profiles, client jar, asset index and the library jars that apply to this
platform — fetched once by the server and stored content-addressed under
`<data>/runtimes/cas/`. Ids are `{mc}--vanilla` or
`{mc}--{loader}--{loader_version}`, so a Fabric and a Quilt pack on the same
version share the client jar blob instead of duplicating it. Asset *objects*
(hundreds of MB) are opt-in with `LAUNCHCRAFT_RUNTIME_ASSETS=1` or per request;
by default only the index is staged and clients keep pulling objects from
Mojang as before.

The console's `GET /console/runtimes` shows every build (status, sizes, which
packs reference it); `POST /console/runtimes` queues one,
`DELETE /console/runtimes/<id>` unregisters it, and `POST /console/runtimes/gc`
collects runtimes no pack references plus the blobs they alone used. Forge and
NeoForge are staged server-side too: their launch profile is read out of the
installer jar's `version.json` (a plain Mojang-format profile), so no
installer run and no JDK are needed — the per-major Java pool covers whatever
the profile declares; those packs keep shipping the installer through
`src/loader/` as a fallback.

### One Server, Many Java Versions

Packs span fifteen years of Minecraft, and each era wants a different JVM:
`1.7.10` / `1.10.2` / `1.12.2` run on **Java 8**, `1.17` on **16**,
`1.18`–`1.20.4` on **17**, `1.20.5`+ on **21**. So Java is pooled by **major
version** — one JDK per major, shared by every runtime and pack that asks for
it, rather than a single global `JAVA_HOME`:

```
<data>/jres/java-8/bin/java      <- for 1.7.10-era packs
<data>/jres/java-17/bin/java     <- for 1.18+ packs
<data>/jres/java-21/bin/java     <- for 1.20.5+ packs
```

Every runtime record carries the `required_java` it needs (the profile's own
`javaVersion.majorVersion` when it declares one, otherwise the version table),
and `GET /console/jres` answers per major: available or not, from where
(`env` / `system` / `managed`), its size, and `required_by` — the runtimes that
would break without it. `POST /console/jres {"major": 8}` fetches one ahead of
time; `DELETE /console/jres/<major>` is refused while a runtime needs it unless
`force=1`.

Resolution order for a major: `LAUNCHCRAFT_JAVA_<major>` → an already
provisioned `<data>/jres/java-<major>` → a **host** JDK (`java` on `PATH`, then
`JAVA_HOME`) — used only when `java -version` confirms the major, and
symlinks are resolved so `/usr/bin/java` is measured as its real JDK home, not
as `/usr` → a sha256-verified Temurin download from Adoptium. Set
`LAUNCHCRAFT_JRES_HOST_JAVA=0` to ignore host JDKs and keep the pool strictly
managed. Minecraft ≤ `1.5.x` wants Java 7/6, which Adoptium no longer
publishes: those majors are listed as non-downloadable and resolve only through
`LAUNCHCRAFT_JAVA_7` (or `_6`), so the console explains it instead of failing
mysteriously. `docker/Dockerfile.server` ships Java 17 as the common case, which
is picked up as a host JDK rather than re-downloaded, and the rest land on the
data volume, so a container restart never fetches a JDK twice.

Enter that same **Server URL** in the launcher (Home tab): the pack list is
pulled automatically on start-up, shows each pack's install state (not
installed / installed / requires update), and **Install** syncs the pack into
`<instances>/<id>/` through `launchcraft-sync` in the background, with a live
progress bar. When the pack names a server-staged runtime, the launcher pulls
that into the shared data root **first** — one `versions/`, `assets/` and
`libraries/` set per Minecraft+loader, hard-linked from the local CAS and shared
by every pack — so **PLAY** works without touching the Versions tab. A runtime
sync is strictly additive: it writes only the files its manifest names, refuses
any path outside `versions/`, `assets/`, `libraries/`, re-verifies what is
already on disk (healing a truncated jar, picking up a server rebuild) and
deletes nothing, because the same directory also holds instances, config and the
cache. Saving the URL also re-points every instance's backup endpoint
at `<url>/api`, so packs and backups always share one server.

The **Java Settings** tab lists every Java runtime it finds on the machine
(JAVA_HOME, `/usr/lib/jvm`, SDKMAN, macOS/Windows install trees) with its
parsed version — pick one from the list or type a custom path.

### Browsing & Adding Modpacks (CurseForge · Modrinth · FTB)

In the console's **Pack Manager**, *Browse Modpacks…* opens one provider-aware
browser (search, game-version filter, sort, version selector) over three
modpack hosts:

| Provider | Upstream | Key | Pack archive |
| :--- | :--- | :--- | :--- |
| CurseForge | `api.curseforge.com/v1` (proxied) | `LAUNCHCRAFT_CURSEFORGE_KEY` | `manifest.json` + referenced mods |
| Modrinth | `api.modrinth.com/v2` | none needed | `.mrpack` with direct-download URLs |
| FTB | legacy catalog on `dist.creeper.host/FTB2` | none needed | baked `minecraft/` zip tree |

All of it is server-side: the browser never sees a key — the console calls
*this* server, which calls the provider. **Add** downloads the chosen pack
file and turns it into a launchcraft pack: resolving and fetching every
referenced mod (CurseForge), downloading the manifest's direct URLs
(Modrinth), or copying the baked tree wholesale (FTB), then merging the
`overrides/`, registering, and building — a normal Pack Manager pack served
to launchers exactly like a hand-built one. A missing **required** mod aborts
the import; optional mods are skipped with a warning. The CurseForge key comes
free from developers.curseforge.org; without it the provider list simply shows
CurseForge as not configured.

Imported packs keep their provenance (`{provider, project, file}`) in the
registry, so the Pack Manager row gains an **Update** button: pick a newer
version (default: the provider's newest) and the server re-downloads and
re-imports it into the same workspace — manifest-based packs have mods the new
version dropped **pruned**, baked FTB packs are wiped and replaced outright —
re-applies the overrides and rebuilds; id and title are kept. Asking for the
version already installed is a no-op. Packs you built by hand have no
provenance and no update button.

Custom packs get their own **Upload Modpack…** button next to the browser:
pick any archive from your machine (CurseForge zip, `.mrpack`, or a baked
`minecraft/` tree), name the pack, and it streams into the identical
import-and-build pipeline — for baked trees the version/game-version fields
fill in the metadata the archive itself doesn't carry. Size-capped by
`LAUNCHCRAFT_MAX_UPLOAD_MB` like everything else; uploaded packs simply never
offer an update, since there is no upstream feed.

Every import also pulls the pack's **mod loader data** into the tree, so a
synced instance can actually launch: Fabric/Quilt packs get their loader
`profile.json` plus every library jar from the loader meta API, Forge and
NeoForge packs get the official installer jar from their Maven — all stored
under `src/loader/`, packed into the content-addressed distribution and
recorded in the manifest (`"loader": "fabric-0.14.13"`). Baked FTB trees
already contain their launcher and are left alone; a loader the server can't
recognise (liteloader, or a pack naming none) is a warning in the response,
never a failed import.

### Playing: Microsoft Login, Versions & Launch (Launcher Client)

The egui client now plays the game it installs, PrismLauncher-style:

- **Accounts tab** — *Sign in with Microsoft* runs the full device-code chain
  (Microsoft → Xbox Live → XSTS → Minecraft services, entitlement check,
  profile + skin) without leaving the launcher: show the one-time code, open
  the sign-in page, and the account lands in `config/accounts.json` with its
  skin face as avatar. Multiple accounts, one active; tokens refresh
  automatically when a launch finds them close to expiry.
- **Versions tab** — the real Mojang version manifest (releases or
  snapshots, filter box), and a **vanilla / Fabric / Quilt** choice with the
  loader's published versions for the selected game. Installs land in the
  standard layout — `versions/<id>/`, `assets/{indexes,objects}/`, shared
  maven-layout `libraries/` — every file sha1-verified, cached, and skipped on
  re-install; loader overlays are installed as `<mc>-<loader>-<version>`
  profiles that inherit the vanilla layer.
- **PLAY (Home tab)** — resolves the selected pack (or version/instance),
  refreshes the token if needed, and launches: rule-filtered classpath
  (overlay libraries first, client jar last), natives unpacked zip-slip-safe,
  modern rule-gated `arguments` *and* legacy `minecraftArguments` fully
  substituted. The game's own console output streams into the Logs tab.

When the version profile requires a newer Java than the one configured on the
Java tab, Launchcraft downloads the matching Temurin build from Adoptium
(sha256-verified, extracted into `runtimes/java-<major>/`) and launches with
it — provisioned runtimes also appear in the Java tab's dropdown. If the
download fails the game still launches with your configured Java.

Forge/NeoForge packs now get their launcher profile staged server-side from the
official installer jar (see *Server-Side Runtime Distribution* above), so a
synced instance launches through the ordinary version-manager path; the server
keeps the installer itself only as a fallback for tooling that wants it.

| Variable | Default | Description |
| :--- | :--- | :--- |
| `LAUNCHCRAFT_META_URL` | `https://piston-meta.mojang.com` | Version-manifest / profile base |
| `LAUNCHCRAFT_FABRIC_URL` | `https://meta.fabricmc.net/v2` | Fabric loader meta base |
| `LAUNCHCRAFT_QUILT_URL` | `https://meta.quiltmc.org/v3` | Quilt loader meta base |
| `LAUNCHCRAFT_RESOURCES_URL` | `https://resources.download.minecraft.net` | Asset object host |
| `LAUNCHCRAFT_ADOPTIUM_URL` | `https://api.adoptium.net/v3` | Temurin JDK assets API (auto Java provisioning) |
| `LAUNCHCRAFT_MS_AUTH_URL` | `https://login.microsoftonline.com/consumers` | Microsoft authority base |
| `LAUNCHCRAFT_CLIENT_ID` | built-in public client | Azure app id for the device flow |

---

## Quickstart & Local Development

### Prerequisites
- [Rust](https://rustup.rs/) (1.85+, earlier toolchains cannot parse current dependencies)
- `libzstd` development libraries:
  - **Debian / Ubuntu:** `sudo apt-get install libzstd-dev`
  - **Fedora:** `sudo dnf install libzstd-devel`
  - **macOS (Apple Silicon):** `brew install zstd`
  - **macOS (Intel / Non-Homebrew):**
    - **MacPorts:** `sudo port install zstd`
    - **pkgsrc:** `pkgin install zstd`
    - **Source build:**
      ```bash
      git clone --depth 1 https://github.com/facebook/zstd.git
      make -C zstd && sudo make -C zstd install
      ```
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

# Run the backup server with its admin console (http://localhost:9090)
cargo run -p launchcraft-server
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
