//! Phase 11 server-side runtime distribution tests. One mock plays piston-meta
//! (`/meta`), a blob CDN (`/files`), maven repos (`/maven`, `/forge-maven`,
//! `/neoforge-maven`) and the asset object host (`/resources`), so a runtime
//! build never leaves the test process. Covers: vanilla, Fabric and Forge
//! builds, the shared CAS (dedup across runtimes), opt-in asset objects,
//! rule-filtered libraries, installer profile guards, unknown-loader refusal,
//! token gating, path fencing, delete-with-references and the gc sweep.

use std::collections::BTreeMap;
use std::io::{Read as _, Write as _};
use std::path::Path;
use std::sync::{Arc, OnceLock};

use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Json;
use sha1::{Digest, Sha1};

use launchcraft_server::packs::{PackEntry, PackRegistry};
use launchcraft_server::{build_router, AppState, ServerConfig};

/// `libraries/com/example/windows-only/…` is `os.name: windows` only, and the
/// tests run on Linux — it must never appear in a runtime manifest.
const WINDOWS_ONLY_PATH: &str = "libraries/com/example/windows-only/1.0/windows-only-1.0.jar";

fn sha1_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha1::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>()
}

/// The mock upstream: named blobs, plus its own base URL (filled after bind).
#[derive(Default)]
struct Mock {
    files: std::sync::Mutex<BTreeMap<String, Vec<u8>>>,
    base: OnceLock<String>,
    /// Every path the maven handlers were asked for, in order.
    served: std::sync::Mutex<Vec<String>>,
    /// Set when the mismatched-Forge installer is requested.
    forge_mismatch: std::sync::atomic::AtomicBool,
}

impl Mock {
    fn put(&self, name: &str, bytes: Vec<u8>) {
        self.files.lock().unwrap().insert(name.into(), bytes);
    }
    fn base(&self) -> &str {
        self.base.get().expect("mock base set before serving")
    }
    fn get(&self, name: &str) -> Option<Vec<u8>> {
        self.files.lock().unwrap().get(name).cloned()
    }
}

/// Build an installer jar: a zip of literal entries. Forge/NeoForge installers
/// are exactly this shape, and only `version.json` matters to the server.
fn installer_zip(entries: &[(&str, &str)]) -> Vec<u8> {
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for (name, body) in entries {
        zip.start_file(*name, options).unwrap();
        zip.write_all(body.as_bytes()).unwrap();
    }
    zip.finish().unwrap().into_inner()
}

/// Forge/NeoForge maven: installers and library jars by repository path.
async fn maven_repo(
    State(m): State<Arc<Mock>>,
    UrlPath(rest): UrlPath<String>,
) -> impl IntoResponse {
    m.served.lock().unwrap().push(rest.clone());
    if rest.contains("MISMATCH") {
        m.forge_mismatch
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
    match m.get(&format!("maven:{rest}")) {
        Some(bytes) => (StatusCode::OK, bytes),
        None => (StatusCode::NOT_FOUND, Vec::new()),
    }
}

async fn blob(State(m): State<Arc<Mock>>, UrlPath(name): UrlPath<String>) -> impl IntoResponse {
    match m.get(&name) {
        Some(bytes) => (StatusCode::OK, bytes),
        None => (StatusCode::NOT_FOUND, Vec::new()),
    }
}

async fn maven_blob(
    State(m): State<Arc<Mock>>,
    UrlPath(rest): UrlPath<String>,
) -> impl IntoResponse {
    match m.get(rest.trim_start_matches('/')) {
        Some(bytes) => (StatusCode::OK, bytes),
        None => (StatusCode::NOT_FOUND, Vec::new()),
    }
}

async fn asset_object(
    State(m): State<Arc<Mock>>,
    UrlPath((_shard, hash)): UrlPath<(String, String)>,
) -> impl IntoResponse {
    match m.get(&format!("asset:{hash}")) {
        Some(bytes) => (StatusCode::OK, bytes),
        None => (StatusCode::NOT_FOUND, Vec::new()),
    }
}

async fn version_manifest(State(m): State<Arc<Mock>>) -> Json<serde_json::Value> {
    let base = m.base().to_string();
    Json(serde_json::json!({
        "latest": { "release": "1.20.1", "snapshot": "1.20.1" },
        "versions": [
            { "id": "1.20.1", "type": "release", "url": format!("{base}/meta/versions/1.20.1") },
        ],
    }))
}

async fn vanilla_profile(State(m): State<Arc<Mock>>) -> impl IntoResponse {
    let base = m.base().to_string();
    let index = serde_json::json!({
        "objects": {
            "minecraft_assets/foo.txt": {
                "hash": sha1_hex(b"asset-bytes"),
                "size": b"asset-bytes".len(),
            }
        }
    });
    let index_bytes = serde_json::to_vec(&index).unwrap();
    m.put("asset-index-5.json", index_bytes.clone());
    m.put(
        &format!("asset:{}", sha1_hex(b"asset-bytes")),
        b"asset-bytes".to_vec(),
    );
    let json = serde_json::json!({
        "id": "1.20.1",
        "mainClass": "net.minecraft.client.main.Main",
        "assets": "5",
        "assetIndex": {
            "id": "5",
            "sha1": sha1_hex(&index_bytes),
            "size": index_bytes.len(),
            "totalSize": b"asset-bytes".len(),
            "url": format!("{base}/meta/assets/5.json"),
        },
        "downloads": { "client": { "url": format!("{base}/files/client.jar") } },
        "libraries": [
            { "name": "com.example:core:1.0",
              "downloads": { "artifact": {
                  "path": "com/example/core/1.0/core-1.0.jar",
                  "url": format!("{base}/files/core.jar"),
              } } },
            { "name": "com.example:windows-only:1.0",
              "rules": [ { "action": "allow", "os": { "name": "windows" } } ],
              "downloads": { "artifact": {
                  "path": "com/example/windows-only/1.0/windows-only-1.0.jar",
                  "url": format!("{base}/files/windows.jar"),
              } } },
        ],
    });
    Json(json)
}

async fn asset_index(State(m): State<Arc<Mock>>) -> impl IntoResponse {
    match m.get("asset-index-5.json") {
        Some(bytes) => (
            StatusCode::OK,
            [("content-type", "application/json")],
            bytes,
        ),
        None => (
            StatusCode::NOT_FOUND,
            [("content-type", "application/json")],
            Vec::new(),
        ),
    }
}

async fn loader_profile(State(m): State<Arc<Mock>>) -> impl IntoResponse {
    let base = m.base().to_string();
    Json(serde_json::json!({
        "id": "1.20.1-fabric-0.15.11",
        "inheritsFrom": "1.20.1",
        "mainClass": "net.fabricmc.loader.impl.launch.knot.KnotClient",
        "libraries": [
            { "name": "net.fabricmc:fabric-loader:0.15.11",
              "url": format!("{base}/maven/"),
              "maven": "net/fabricmc/fabric-loader/0.15.11/fabric-loader-0.15.11.jar" },
        ],
    }))
}

/// Mock upstream + launchcraft-server, both on ephemeral ports.
async fn spawn(data_dir: &Path, token: Option<String>) -> (String, Arc<Mock>) {
    let mock = Arc::new(Mock::default());
    mock.put("client.jar", b"vanilla-client-jar".to_vec());
    mock.put("core.jar", b"core-library".to_vec());
    mock.put("windows.jar", b"windows-only-bytes".to_vec());
    mock.put("example-mod.jar", b"a-mod-jar".to_vec());
    mock.put(
        "net/fabricmc/fabric-loader/0.15.11/fabric-loader-0.15.11.jar",
        b"fabric-loader-jar".to_vec(),
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mock_base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    mock.base.set(mock_base.clone()).ok();

    // Forge-family installers, addressed the way the real mavens address them.
    // `version.json` is a Mojang profile; `install_profile.json` lists the
    // INSTALLER's own dependencies, which must never reach a runtime manifest.
    mock.put("asm.jar", b"asm-bytecode-library".to_vec());
    mock.put(
        "maven:net/minecraftforge/forge/1.20.1-47.2.0/forge-1.20.1-47.2.0.jar",
        b"forge-universal-jar".to_vec(),
    );
    let forge_profile = r#"{
        "id": "1.20.1-forge-47.2.0",
        "inheritsFrom": "1.20.1",
        "mainClass": "cpw.mods.bootstraplauncher.BootstrapLauncher",
        "libraries": [
          { "name": "net.minecraftforge:forge:1.20.1-47.2.0",
            "downloads": { "artifact": {
              "path": "net/minecraftforge/forge/1.20.1-47.2.0/forge-1.20.1-47.2.0.jar",
              "url": "" } } },
          { "name": "org.ow2.asm:asm:9.5",
            "downloads": { "artifact": {
              "path": "org/ow2/asm/asm/9.5/asm-9.5.jar",
              "url": "BASE/files/asm.jar" } } }
        ]
    }"#
    .replace("BASE", &mock_base);
    mock.put(
        "maven:net/minecraftforge/forge/1.20.1-47.2.0/forge-1.20.1-47.2.0-installer.jar",
        installer_zip(&[
            ("version.json", &forge_profile),
            (
                "install_profile.json",
                r#"{"version":"1.20.1-forge-47.2.0","libraries":[{"name":"com.github.jponge:lzma-java:1.3","downloads":{"artifact":{"path":"com/github/jponge/lzma-java/1.3/lzma-java-1.3.jar","url":"http://example.invalid/x.jar"}}}],"processors":[]}"#,
            ),
            ("data/client.lzma", "installer payload, never a game file"),
        ]),
    );

    // Same coordinates, profile for another Minecraft release.
    let mismatch_profile =
        forge_profile.replace(r#""inheritsFrom": "1.20.1""#, r#""inheritsFrom": "1.20.4""#);
    mock.put(
        "maven:net/minecraftforge/forge/1.20.1-MISMATCH/forge-1.20.1-MISMATCH-installer.jar",
        installer_zip(&[("version.json", &mismatch_profile)]),
    );

    // NeoForge-style installer with no profile inside.
    mock.put(
        "maven:net/neoforged/neoforge/NOPROFILE/neoforge-NOPROFILE-installer.jar",
        installer_zip(&[
            ("install_profile.json", "{}"),
            ("META-INF/MANIFEST.MF", "Manifest-Version: 1.0"),
        ]),
    );

    let app = axum::Router::new()
        .route(
            "/meta/mc/game/version_manifest_v2.json",
            get(version_manifest),
        )
        .route("/meta/versions/:id", get(vanilla_profile))
        .route("/meta/assets/:id", get(asset_index))
        .route(
            "/fabric/versions/loader/:mc/:lv/profile/json",
            get(loader_profile),
        )
        .route("/files/:name", get(blob))
        .route("/maven/*rest", get(maven_blob))
        .route("/resources/:shard/:hash", get(asset_object))
        .route("/forge-maven/*rest", get(maven_repo))
        .route("/neoforge-maven/*rest", get(maven_repo))
        .with_state(mock.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let cfg = ServerConfig {
        addr: "127.0.0.1:0".into(),
        data_dir: data_dir.to_path_buf(),
        token,
        repo_dist: None,
        max_upload_mb: 8,
        curseforge_key: None,
        curseforge_url: "https://api.curseforge.com/v1".into(),
        modrinth_url: format!("{mock_base}/mr/v2"),
        ftb_url: format!("{mock_base}/ftb"),
        fabric_url: format!("{mock_base}/fabric"),
        quilt_url: format!("{mock_base}/quilt"),
        forge_url: format!("{mock_base}/forge-maven"),
        neoforge_url: format!("{mock_base}/neoforge-maven"),
        meta_url: format!("{mock_base}/meta"),
        resources_url: format!("{mock_base}/resources"),
        adoptium_url: format!("{mock_base}/adoptium/v3"),
        ..Default::default()
    };
    let state = AppState::new(&cfg).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    tokio::spawn(async move { axum::serve(listener, build_router(state)).await.unwrap() });
    (base, mock)
}

fn headers(token: Option<&str>) -> reqwest::header::HeaderMap {
    let mut h = reqwest::header::HeaderMap::new();
    if let Some(t) = token {
        h.insert("authorization", format!("Bearer {t}").parse().unwrap());
    }
    h
}

async fn get_json(base: &str, path: &str) -> serde_json::Value {
    get_json_auth(base, path, None).await
}

async fn get_json_auth(base: &str, path: &str, token: Option<&str>) -> serde_json::Value {
    let res = reqwest::Client::new()
        .get(format!("{base}{path}"))
        .headers(headers(token))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200, "GET {path}");
    res.json().await.unwrap()
}

async fn queue(
    base: &str,
    token: Option<&str>,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let res = reqwest::Client::new()
        .post(format!("{base}/console/runtimes"))
        .headers(headers(token))
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = res.status();
    let body = res
        .json::<serde_json::Value>()
        .await
        .unwrap_or(serde_json::json!({}));
    (status, body)
}

async fn console_runtimes(base: &str) -> serde_json::Value {
    console_runtimes_auth(base, None).await
}

async fn console_runtimes_auth(base: &str, token: Option<&str>) -> serde_json::Value {
    get_json_auth(base, "/console/runtimes", token).await
}

fn find_runtime<'a>(list: &'a serde_json::Value, id: &str) -> &'a serde_json::Value {
    list["runtimes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == serde_json::json!(id))
        .unwrap_or_else(|| panic!("runtime {id} missing in {list}"))
}

/// Wait for a background build to leave `building`, whatever the outcome.
async fn wait_settled(base: &str, id: &str) -> serde_json::Value {
    for _ in 0..400 {
        let entry = find_runtime(&console_runtimes(base).await, id).clone();
        if entry["status"] != serde_json::json!("building") {
            return entry;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("runtime {id} never settled");
}

async fn wait_ready(base: &str, id: &str) -> serde_json::Value {
    wait_ready_auth(base, id, None).await
}

async fn wait_ready_auth(base: &str, id: &str, token: Option<&str>) -> serde_json::Value {
    for _ in 0..400 {
        let entry = find_runtime(&console_runtimes_auth(base, token).await, id).clone();
        match entry["status"].as_str() {
            Some("ready") => return entry,
            Some("building") => tokio::time::sleep(std::time::Duration::from_millis(25)).await,
            Some(other) => panic!("runtime {id} failed as {other}: {entry}"),
            None => panic!("runtime {id} has no status: {entry}"),
        }
    }
    panic!("runtime {id} never became ready");
}

fn cas_object_count(data_dir: &Path) -> usize {
    let cas = data_dir.join("runtimes").join("cas").join("objects");
    if !cas.is_dir() {
        return 0;
    }
    walkdir::WalkDir::new(&cas)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .count()
}

fn zstd_to_vec(bytes: &[u8]) -> Vec<u8> {
    let mut decoder = zstd::stream::read::Decoder::new(bytes).unwrap();
    let mut out = Vec::new();
    decoder.read_to_end(&mut out).unwrap();
    out
}

// ---------------------------------------------------------------------- tests

#[tokio::test]
async fn vanilla_runtime_is_fetched_packed_and_served() {
    let data = tempfile::tempdir().unwrap();
    let (base, _mock) = spawn(data.path(), None).await;

    let (status, queued) =
        queue(&base, None, serde_json::json!({ "game_version": "1.20.1" })).await;
    assert_eq!(status, 202);
    assert_eq!(queued["id"], "1.20.1--vanilla");
    assert_eq!(queued["status"], "building");

    let entry = wait_ready(&base, "1.20.1--vanilla").await;
    assert_eq!(entry["install_version"], "1.20.1");
    assert_eq!(entry["assets"], "index");
    // profile json + client jar + one applicable library.
    assert_eq!(entry["file_count"], 3, "entry: {entry}");

    let manifest = get_json(&base, "/runtimes/1.20.1--vanilla/manifest.json").await;
    assert_eq!(manifest["pack_id"], "1.20.1--vanilla");
    assert_eq!(manifest["pack_version"], "1.20.1");
    assert_eq!(manifest["game_version"], "1.20.1");
    assert!(manifest["loader"].is_null());
    let files = manifest["files"].as_object().unwrap();
    assert!(files.contains_key("versions/1.20.1/1.20.1.json"));
    assert!(files.contains_key("versions/1.20.1/1.20.1.jar"));
    assert!(files.contains_key("libraries/com/example/core/1.0/core-1.0.jar"));
    assert!(
        !files.contains_key(WINDOWS_ONLY_PATH),
        "rule-filtered library leaked into the runtime"
    );
    assert!(!files.keys().any(|p| p.starts_with("assets/objects/")));
    assert!(!files.contains_key("assets/indexes/5.json"));

    // The listed blob decodes back to the bytes it was built from.
    let jar = &files["versions/1.20.1/1.20.1.jar"];
    let hash = jar["hash"].as_str().unwrap();
    let res = reqwest::Client::new()
        .get(format!(
            "{base}/runtimes/1.20.1--vanilla/cas/objects/{}/{hash}",
            &hash[..2]
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let decoded = zstd_to_vec(&res.bytes().await.unwrap());
    assert_eq!(decoded, b"vanilla-client-jar");
    assert_eq!(blake3::hash(&decoded).to_hex().to_string(), hash);
    assert_eq!(jar["size"], 18);

    // Public list shows the ready runtime with its stats.
    let public = get_json(&base, "/runtimes").await;
    assert_eq!(public["runtimes"].as_array().unwrap().len(), 1);
    assert_eq!(public["runtimes"][0]["id"], "1.20.1--vanilla");

    // Staging trees are not left behind.
    assert!(!data
        .path()
        .join("runtimes/staging/1.20.1--vanilla")
        .exists());
}

#[tokio::test]
async fn fabric_runtime_shares_vanilla_blobs() {
    let data = tempfile::tempdir().unwrap();
    let (base, _mock) = spawn(data.path(), None).await;

    queue(&base, None, serde_json::json!({ "game_version": "1.20.1" })).await;
    wait_ready(&base, "1.20.1--vanilla").await;
    let after_vanilla = cas_object_count(data.path());

    let (status, queued) = queue(
        &base,
        None,
        serde_json::json!({
            "game_version": "1.20.1", "loader": "fabric", "loader_version": "0.15.11",
        }),
    )
    .await;
    assert_eq!(status, 202);
    assert_eq!(queued["id"], "1.20.1--fabric--0.15.11");
    let entry = wait_ready(&base, "1.20.1--fabric--0.15.11").await;
    assert_eq!(entry["install_version"], "1.20.1-fabric-0.15.11");
    assert_eq!(entry["loader"], "fabric");
    assert_eq!(entry["loader_version"], "0.15.11");

    let manifest = get_json(&base, "/runtimes/1.20.1--fabric--0.15.11/manifest.json").await;
    assert_eq!(manifest["loader"], "fabric-0.15.11");
    let files = manifest["files"].as_object().unwrap();
    // Overlay profile + its jar copy + the loader library, on top of the
    // inherited vanilla profile and library.
    assert!(files.contains_key("versions/1.20.1-fabric-0.15.11/1.20.1-fabric-0.15.11.json"));
    assert!(files.contains_key("versions/1.20.1-fabric-0.15.11/1.20.1-fabric-0.15.11.jar"));
    assert!(files
        .contains_key("libraries/net/fabricmc/fabric-loader/0.15.11/fabric-loader-0.15.11.jar"));
    assert!(files.contains_key("versions/1.20.1/1.20.1.json"));

    // The overlay's jar copy is the *same content*, so it is the same blob.
    let vanilla = get_json(&base, "/runtimes/1.20.1--vanilla/manifest.json").await;
    let vanilla_jar = vanilla["files"]["versions/1.20.1/1.20.1.jar"]["hash"]
        .as_str()
        .unwrap();
    let overlay_jar = files["versions/1.20.1-fabric-0.15.11/1.20.1-fabric-0.15.11.jar"]["hash"]
        .as_str()
        .unwrap();
    assert_eq!(vanilla_jar, overlay_jar);

    // Only the genuinely new files cost storage: 3 new blobs (loader profile,
    // loader library, and the re-fetched loader json), not the whole runtime.
    let after_fabric = cas_object_count(data.path());
    assert!(
        after_fabric - after_vanilla <= 3,
        "shared CAS grew too much: {after_vanilla} -> {after_fabric}"
    );
    assert!(after_fabric > after_vanilla);

    // Both runtimes are listed and addressable.
    let public = get_json(&base, "/runtimes").await;
    assert_eq!(public["runtimes"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn rebuilding_a_ready_runtime_is_free_unless_forced() {
    let data = tempfile::tempdir().unwrap();
    let (base, _mock) = spawn(data.path(), None).await;

    queue(&base, None, serde_json::json!({ "game_version": "1.20.1" })).await;
    wait_ready(&base, "1.20.1--vanilla").await;

    // Second request returns the ready record without a new build.
    let (status, again) = queue(&base, None, serde_json::json!({ "game_version": "1.20.1" })).await;
    assert_eq!(status, 200);
    assert_eq!(again["status"], "ready");

    // force=1 queues the build again; a duplicate request that arrives while
    // it is still running joins it instead of starting a second fetch.
    let (status, forced) = queue(
        &base,
        None,
        serde_json::json!({ "game_version": "1.20.1", "force": true }),
    )
    .await;
    assert_eq!(status, 202);
    assert_eq!(forced["status"], "building");
    let (status, second) = queue(
        &base,
        None,
        serde_json::json!({ "game_version": "1.20.1", "force": true }),
    )
    .await;
    assert!(
        matches!(u16::from(status), 200 | 202),
        "duplicate request must join or return the ready runtime, got {status}: {second}"
    );
    if second["status"] == serde_json::json!("building") {
        assert_eq!(
            second["queued"], false,
            "a running build is joined, not restarted"
        );
    }
    let entry = wait_ready(&base, "1.20.1--vanilla").await;
    assert_eq!(
        entry["file_count"], 3,
        "rebuild must be idempotent: {entry}"
    );
}

#[tokio::test]
async fn asset_objects_are_opt_in() {
    let data = tempfile::tempdir().unwrap();
    let (base, _mock) = spawn(data.path(), None).await;

    let (_, queued) = queue(
        &base,
        None,
        serde_json::json!({ "game_version": "1.20.1", "assets": true }),
    )
    .await;
    assert_eq!(queued["id"], "1.20.1--vanilla");
    let entry = wait_ready(&base, "1.20.1--vanilla").await;
    assert_eq!(entry["assets"], "full");

    let manifest = get_json(&base, "/runtimes/1.20.1--vanilla/manifest.json").await;
    let files = manifest["files"].as_object().unwrap();
    assert!(files.contains_key("assets/indexes/5.json"));
    let object_path = format!(
        "assets/objects/{}/{}",
        &sha1_hex(b"asset-bytes")[..2],
        sha1_hex(b"asset-bytes")
    );
    let object = files
        .get(&object_path)
        .unwrap_or_else(|| panic!("missing {object_path} in {:?}", files.keys()));
    // Asset objects are named by sha1 upstream but content-addressed by blake3
    // here, so the CAS hash is not the sha1.
    assert_eq!(object["size"], b"asset-bytes".len() as u64);
    let hash = object["hash"].as_str().unwrap();
    let res = reqwest::Client::new()
        .get(format!(
            "{base}/runtimes/1.20.1--vanilla/cas/objects/{}/{hash}",
            &hash[..2]
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(zstd_to_vec(&res.bytes().await.unwrap()), b"asset-bytes");
}

#[tokio::test]
async fn forge_runtime_is_staged_from_the_installer_jar() {
    let data = tempfile::tempdir().unwrap();
    let (base, mock) = spawn(data.path(), None).await;

    // Vanilla first, so the Forge runtime can be compared against it.
    queue(&base, None, serde_json::json!({ "game_version": "1.20.1" })).await;
    wait_ready(&base, "1.20.1--vanilla").await;
    let vanilla_blobs = cas_object_count(data.path());

    let (status, queued) = queue(
        &base,
        None,
        serde_json::json!({
            "game_version": "1.20.1", "loader": "forge", "loader_version": "47.2.0",
        }),
    )
    .await;
    assert_eq!(status, 202);
    assert_eq!(queued["id"], "1.20.1--forge--47.2.0");
    let entry = wait_ready(&base, "1.20.1--forge--47.2.0").await;
    assert_eq!(entry["install_version"], "1.20.1-forge-47.2.0");
    assert_eq!(entry["loader"], "forge");

    let manifest = get_json(&base, "/runtimes/1.20.1--forge--47.2.0/manifest.json").await;
    let files = manifest["files"].as_object().unwrap();
    assert_eq!(manifest["loader"], "forge-47.2.0");
    // Profile + client jar copy, the inherited vanilla profile, and both
    // libraries — including the forge jar whose URL the installer left blank.
    assert!(files.contains_key("versions/1.20.1-forge-47.2.0/1.20.1-forge-47.2.0.json"));
    assert!(files.contains_key("versions/1.20.1-forge-47.2.0/1.20.1-forge-47.2.0.jar"));
    assert!(files.contains_key("versions/1.20.1/1.20.1.json"));
    assert!(files
        .contains_key("libraries/net/minecraftforge/forge/1.20.1-47.2.0/forge-1.20.1-47.2.0.jar"));
    assert!(files.contains_key("libraries/org/ow2/asm/asm/9.5/asm-9.5.jar"));
    // The overlay's client jar is the base layer's download (the merged
    // client jar): the forge runtime reuses the vanilla jar bytes, so the
    // shared CAS sees the same blob instead of a second copy.
    let vanilla = get_json(&base, "/runtimes/1.20.1--vanilla/manifest.json").await;
    assert_eq!(
        vanilla["files"]["versions/1.20.1/1.20.1.jar"]["hash"],
        manifest["files"]["versions/1.20.1-forge-47.2.0/1.20.1-forge-47.2.0.jar"]["hash"]
    );

    // The installer itself is never stored as a game file.
    assert!(!files.keys().any(|k| k.contains("installer")));
    // Neither are the installer's own dependencies.
    assert!(!files
        .keys()
        .any(|k| k.contains("lzma-java") || k.contains("javaxdelta")));
    assert!(mock.served.lock().unwrap().contains(
        &"net/minecraftforge/forge/1.20.1-47.2.0/forge-1.20.1-47.2.0-installer.jar".to_string()
    ));

    // Cross-family dedup: the Forge runtime reuses every vanilla blob, so the
    // shared store only grew by what Forge actually adds.
    let after_forge = cas_object_count(data.path());
    let forge_files = manifest["files"].as_object().unwrap().len();
    assert!(
        after_forge - vanilla_blobs < forge_files,
        "expected dedup against vanilla: {vanilla_blobs} -> {after_forge} for {forge_files} entries"
    );
}

#[tokio::test]
async fn forge_installer_for_a_different_minecraft_is_refused() {
    let data = tempfile::tempdir().unwrap();
    let (base, mock) = spawn(data.path(), None).await;

    // Ask for a Forge runtime whose installer belongs to another MC version.
    let (status, queued) = queue(
        &base,
        None,
        serde_json::json!({
            "game_version": "1.20.1", "loader": "forge", "loader_version": "MISMATCH",
        }),
    )
    .await;
    assert_eq!(status, 202);
    assert_eq!(queued["id"], "1.20.1--forge--MISMATCH");
    let entry = wait_settled(&base, "1.20.1--forge--MISMATCH").await;
    assert_eq!(entry["status"], "error");
    assert!(entry["error"]
        .as_str()
        .unwrap()
        .contains("is a profile for Minecraft 1.20.4, not 1.20.1"));
    assert!(mock
        .forge_mismatch
        .load(std::sync::atomic::Ordering::SeqCst));
}

#[tokio::test]
async fn an_installer_without_a_profile_fails_cleanly() {
    let data = tempfile::tempdir().unwrap();
    let (base, _mock) = spawn(data.path(), None).await;

    let (status, _queued) = queue(
        &base,
        None,
        serde_json::json!({
            "game_version": "1.20.1", "loader": "neoforge", "loader_version": "NOPROFILE",
        }),
    )
    .await;
    assert_eq!(status, 202);
    let entry = wait_settled(&base, "1.20.1--neoforge--NOPROFILE").await;
    assert_eq!(entry["status"], "error");
    let message = entry["error"].as_str().unwrap();
    assert!(message.contains("no version.json"), "message: {message}");
    // Nothing half-installed: no manifest, and the staging tree is gone.
    assert!(!data
        .path()
        .join("runtimes/1.20.1--neoforge--NOPROFILE/manifest.json")
        .exists());
    assert!(!data
        .path()
        .join("runtimes/staging/1.20.1--neoforge--NOPROFILE")
        .exists());
}

#[tokio::test]
async fn unknown_loader_kinds_are_refused_before_any_state_change() {
    let data = tempfile::tempdir().unwrap();
    let (base, _mock) = spawn(data.path(), None).await;

    let (status, body) = queue(
        &base,
        None,
        serde_json::json!({
            "game_version": "1.20.1", "loader": "liteloader", "loader_version": "1.12",
        }),
    )
    .await;
    assert_eq!(status, 400);
    let message = body["message"].as_str().unwrap();
    assert!(message.contains("not one of them"), "message: {message}");
    // Rejected before any state change: no half-built record exists.
    assert!(console_runtimes(&base).await["runtimes"]
        .as_array()
        .unwrap()
        .is_empty());

    // A Forge runtime with no explicit version cannot be addressed either.
    let (status, body) = queue(
        &base,
        None,
        serde_json::json!({ "game_version": "1.20.1", "loader": "forge" }),
    )
    .await;
    assert_eq!(status, 400);
    assert!(body["message"].as_str().unwrap().contains("loader_version"));

    // Unsafe ids never reach the filesystem either.
    let (status, _) = queue(&base, None, serde_json::json!({ "game_version": "../etc" })).await;
    assert_eq!(status, 400);
}

#[tokio::test]
async fn runtime_endpoints_fence_paths_and_token() {
    let data = tempfile::tempdir().unwrap();
    let (base, _mock) = spawn(data.path(), Some("secret".to_string())).await;

    // Console routes need the token; the client-facing ones do not.
    let res = reqwest::Client::new()
        .get(format!("{base}/console/runtimes"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 401);
    queue(
        &base,
        Some("secret"),
        serde_json::json!({ "game_version": "1.20.1" }),
    )
    .await;
    assert_eq!(
        reqwest::Client::new()
            .post(format!("{base}/console/runtimes"))
            .json(&serde_json::json!({ "game_version": "1.20.1" }))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        reqwest::Client::new()
            .get(format!("{base}/runtimes"))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    wait_ready_auth(&base, "1.20.1--vanilla", Some("secret")).await;

    // Unknown runtime manifest → 404, not a directory read.
    assert_eq!(
        reqwest::Client::new()
            .get(format!("{base}/runtimes/nope--vanilla/manifest.json"))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );

    // Blob route: shard must match the hash prefix and the hash be lowercase hex.
    let bad_shard = reqwest::Client::new()
        .get(format!(
            "{base}/runtimes/1.20.1--vanilla/cas/objects/ff/{}",
            "0".repeat(64)
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(bad_shard.status(), 400);
    let upper = reqwest::Client::new()
        .get(format!(
            "{base}/runtimes/1.20.1--vanilla/cas/objects/ab/{}",
            "AB".repeat(32)
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(upper.status(), 400);
    let traversal = reqwest::Client::new()
        .get(format!(
            "{base}/runtimes/..%2f..%2fpacks/cas/objects/ab/{}",
            "a".repeat(64)
        ))
        .send()
        .await
        .unwrap();
    assert!(traversal.status() == 400 || traversal.status() == 404);
}

#[tokio::test]
async fn delete_respects_pack_references_and_gc_reclaims_orphans() {
    let data = tempfile::tempdir().unwrap();
    let (base, _mock) = spawn(data.path(), None).await;

    queue(&base, None, serde_json::json!({ "game_version": "1.20.1" })).await;
    wait_ready(&base, "1.20.1--vanilla").await;
    assert!(cas_object_count(data.path()) > 0);

    // A pack pointing at the runtime blocks the delete.
    let mut registry = PackRegistry::default();
    registry
        .add(PackEntry {
            id: "alice".into(),
            title: "Alice".into(),
            src: None,
            dist: data.path().join("packs/alice/dist"),
            created_at: String::new(),
            source: None,
            runtime_id: Some("1.20.1--vanilla".into()),
        })
        .unwrap();
    registry.save(data.path()).unwrap();

    let res = reqwest::Client::new()
        .delete(format!("{base}/console/runtimes/1.20.1--vanilla"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 400);
    assert!(res.text().await.unwrap().contains("alice"));

    // gc keeps referenced runtimes and their blobs.
    let kept = reqwest::Client::new()
        .post(format!("{base}/console/runtimes/gc"))
        .send()
        .await
        .unwrap();
    assert_eq!(kept.status(), 200);
    let kept = kept.json::<serde_json::Value>().await.unwrap();
    assert_eq!(
        kept["kept_runtimes"],
        serde_json::json!(["1.20.1--vanilla"])
    );
    assert_eq!(kept["dropped_runtimes"], serde_json::json!([]));
    assert!(cas_object_count(data.path()) > 0);

    // Drop the reference: the runtime and every blob become collectable.
    let mut registry = PackRegistry::default();
    registry
        .add(PackEntry {
            id: "alice".into(),
            title: "Alice".into(),
            src: None,
            dist: data.path().join("packs/alice/dist"),
            created_at: String::new(),
            source: None,
            runtime_id: None,
        })
        .unwrap();
    registry.save(data.path()).unwrap();

    let swept = reqwest::Client::new()
        .post(format!("{base}/console/runtimes/gc"))
        .send()
        .await
        .unwrap();
    assert_eq!(swept.status(), 200);
    let swept = swept.json::<serde_json::Value>().await.unwrap();
    assert_eq!(
        swept["dropped_runtimes"],
        serde_json::json!(["1.20.1--vanilla"])
    );
    assert!(swept["dropped_blobs"].as_u64().unwrap() >= 3);
    assert!(swept["reclaimed_bytes"].as_u64().unwrap() > 0);
    assert_eq!(cas_object_count(data.path()), 0);
    assert!(!data
        .path()
        .join("runtimes/1.20.1--vanilla/manifest.json")
        .exists());
    // Public listing is empty again.
    assert_eq!(
        get_json(&base, "/runtimes").await["runtimes"],
        serde_json::json!([])
    );
}

#[tokio::test]
async fn failed_builds_record_an_error_instead_of_a_ready_runtime() {
    let data = tempfile::tempdir().unwrap();
    let (base, _mock) = spawn(data.path(), None).await;

    // The mock's version list only knows 1.20.1.
    let (status, queued) = queue(&base, None, serde_json::json!({ "game_version": "1.9.4" })).await;
    assert_eq!(status, 202);
    assert_eq!(queued["id"], "1.9.4--vanilla");

    let entry = wait_settled(&base, "1.9.4--vanilla").await;
    assert_eq!(entry["status"], "error", "entry: {entry}");
    assert!(entry["error"]
        .as_str()
        .unwrap()
        .contains("not in the Minecraft version manifest"));
    // Broken runtimes stay out of the client-facing list.
    assert_eq!(
        get_json(&base, "/runtimes").await["runtimes"],
        serde_json::json!([])
    );
    assert!(!data
        .path()
        .join("runtimes/1.9.4--vanilla/manifest.json")
        .exists());
    // No staging tree is left behind after a failure either.
    assert!(!data.path().join("runtimes/staging/1.9.4--vanilla").exists());

    // gc clears the failed record so the id can be retried.
    let swept = reqwest::Client::new()
        .post(format!("{base}/console/runtimes/gc"))
        .send()
        .await
        .unwrap();
    let swept = swept.json::<serde_json::Value>().await.unwrap();
    assert_eq!(
        swept["dropped_runtimes"],
        serde_json::json!(["1.9.4--vanilla"])
    );
    assert!(console_runtimes(&base).await["runtimes"]
        .as_array()
        .unwrap()
        .is_empty());
}

/// A one-mod `.mrpack` whose dependencies name Fabric + Minecraft 1.20.1, so
/// the import pipeline must derive the runtime id and queue that build.
fn mrpack_archive(mock_base: &str) -> Vec<u8> {
    let index = serde_json::json!({
        "game": "minecraft",
        "formatVersion": 1,
        "versionId": "1.0.0",
        "name": "Demo Fabric Pack",
        "dependencies": { "minecraft": "1.20.1", "fabric-loader": "^0.15.11" },
        "files": [
            { "path": "mods/example-mod.jar",
              "hashes": { "sha1": sha1_hex(b"a-mod-jar") },
              "downloads": [format!("{mock_base}/files/example-mod.jar")],
              "env": { "client": "required", "server": "required" } },
        ],
    });
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    zip.start_file("modrinth.index.json", options).unwrap();
    zip.write_all(&serde_json::to_vec(&index).unwrap()).unwrap();
    zip.start_file("overrides/options.txt", options).unwrap();
    zip.write_all(b"lang=en_us\n").unwrap();
    zip.finish().unwrap().into_inner()
}

#[tokio::test]
async fn importing_a_pack_queues_and_records_its_runtime() {
    let data = tempfile::tempdir().unwrap();
    let (base, mock) = spawn(data.path(), None).await;
    let archive = mrpack_archive(mock.base());

    let res = reqwest::Client::new()
        .post(format!("{base}/console/modpacks/upload?id=demo&title=Demo"))
        .header("content-type", "application/zip")
        .body(archive)
        .send()
        .await
        .unwrap();
    assert_eq!(
        res.status(),
        200,
        "upload failed: {:?}",
        res.text().await.ok()
    );
    let imported = res.json::<serde_json::Value>().await.unwrap();
    assert_eq!(imported["loader"], "fabric-0.15.11");
    assert_eq!(imported["runtime"], "1.20.1--fabric--0.15.11");

    // The runtime reaches ready and the pack registry keeps the link.
    let entry = wait_ready(&base, "1.20.1--fabric--0.15.11").await;
    assert!(entry["file_count"].as_u64().unwrap() >= 5);
    let packs = get_json(&base, "/console/packs").await;
    let pack = packs["packs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == serde_json::json!("demo"))
        .expect("imported pack listed");
    assert_eq!(pack["runtime_id"], "1.20.1--fabric--0.15.11");

    // The client-facing pack list carries it too, so a launcher can resolve
    // the runtime alongside the pack without a second round trip.
    let public = get_json(&base, "/packs").await;
    assert_eq!(public["packs"][0]["runtime_id"], "1.20.1--fabric--0.15.11");

    // gc now keeps the runtime: a pack references it.
    let swept = reqwest::Client::new()
        .post(format!("{base}/console/runtimes/gc"))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(
        swept["kept_runtimes"],
        serde_json::json!(["1.20.1--fabric--0.15.11"])
    );
    assert_eq!(swept["dropped_runtimes"], serde_json::json!([]));
}
