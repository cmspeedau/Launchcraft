//! CurseForge import tests against a mock of the Core API: search, version
//! listing, and the full Add pipeline (modpack zip → manifest → mod files →
//! overrides → registry → build), for both Forge (`manifest.json`) and
//! Fabric (`modrinth.index.json`) pack layouts.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::sync::{Arc, Mutex, OnceLock};

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Json;
use launchcraft_server::{build_router, AppState, ServerConfig};

const KEY: &str = "test-cf-key";

/// A stand-in for api.curseforge.com: `/v1/*` JSON (x-api-key gated) plus
/// `/cdn/<name>` blobs. `base` is the mock's own URL, filled after binding so
/// download links can point back at this very server.
#[derive(Default)]
struct Mock {
    files: Mutex<BTreeMap<String, Vec<u8>>>,
    searches: Mutex<Vec<String>>,
    base: OnceLock<String>,
}

impl Mock {
    fn base(&self) -> &str {
        self.base.get().expect("mock base set before serving")
    }
    fn check_key(&self, headers: &HeaderMap) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
        let given = headers
            .get("x-api-key")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        if given == KEY {
            Ok(())
        } else {
            Err((
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({ "message": "Key invalid" })),
            ))
        }
    }
}

async fn cdn(State(m): State<Arc<Mock>>, Path(name): Path<String>) -> impl IntoResponse {
    let files = m.files.lock().unwrap();
    match files.get(&name) {
        Some(bytes) => (StatusCode::OK, bytes.clone()),
        None => (StatusCode::NOT_FOUND, Vec::new()),
    }
}

async fn search(
    State(m): State<Arc<Mock>>,
    headers: HeaderMap,
    Query(q): Query<BTreeMap<String, String>>,
) -> impl IntoResponse {
    if let Err(e) = m.check_key(&headers) {
        return e.into_response();
    }
    // Modpack browsing must scope to the Minecraft Modpacks class.
    assert_eq!(q.get("classId").map(String::as_str), Some("4471"));
    m.searches.lock().unwrap().push(
        q.get("searchTerms")
            .cloned()
            .unwrap_or_else(|| "(none)".into()),
    );
    let json = serde_json::json!({
        "data": [
            {
                "id": 1000, "name": "Adventure Wasteland",
                "summary": "You are stranded in a Fallout Wasteland, But, You ar...",
                "imageUrl": { "url": "//cdn.example/icon1.png" },
                "downloadCount": 123456,
            },
            {
                "id": 1100, "name": "Fabric Fields",
                "summary": "A tidy fabric pack.",
                "imageUrl": { "url": "https://cdn.example/icon2.png" },
                "downloadCount": 7,
            },
        ],
        "pagination": { "index": 0, "pageSize": 30, "resultCount": 2, "totalCount": 2 },
    });
    Json(json).into_response()
}

async fn mod_files(
    State(m): State<Arc<Mock>>,
    headers: HeaderMap,
    Path(id): Path<u64>,
) -> impl IntoResponse {
    if let Err(e) = m.check_key(&headers) {
        return e.into_response();
    }
    let json = match id {
        1000 => serde_json::json!({ "data": [{
            "id": 5001, "displayName": "Adventure 1.0.0", "fileName": "adventure-1.0.0.zip",
            "fileLength": 2048, "releaseDate": "2026-05-01T00:00:00Z",
            "gameVersions": ["1.20.1", "Forge"],
        }] }),
        1100 => serde_json::json!({ "data": [{
            "id": 5101, "displayName": "Fields 0.3", "fileName": "fields.zip",
            "fileLength": 1024, "releaseDate": "2026-06-01T00:00:00Z",
            "gameVersions": ["1.21.4", "Fabric"],
        }] }),
        _ => serde_json::json!({ "data": [] }),
    };
    Json(json).into_response()
}

async fn pack_download(
    State(m): State<Arc<Mock>>,
    headers: HeaderMap,
    Path((id, file)): Path<(u64, u64)>,
) -> impl IntoResponse {
    if let Err(e) = m.check_key(&headers) {
        return e.into_response();
    }
    let cdn_name = match (id, file) {
        (1000, 5001) => "adventure.zip",
        (1100, 5101) => "fields.zip",
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    Json(serde_json::json!({ "data": {
        "cdnUrl": format!("{}/cdn/{cdn_name}", m.base()),
        "fileName": cdn_name,
    }}))
    .into_response()
}

async fn file_info(
    State(m): State<Arc<Mock>>,
    headers: HeaderMap,
    Path((project, file)): Path<(u64, u64)>,
) -> impl IntoResponse {
    if let Err(e) = m.check_key(&headers) {
        return e.into_response();
    }
    let json = match (project, file) {
        (201, 9001) => serde_json::json!({ "data": {
            "downloadUrl": format!("{}/cdn/alpha.jar", m.base()),
            "fileName": "alpha-mod-1.2.jar",
        }}),
        (202, 9002) => serde_json::json!({ "data": {
            "downloadUrl": format!("{}/cdn/beta.jar", m.base()),
            "fileName": "beta mod 2.jar",
        }}),
        // Listed as optional, but its blob is never served: exercises the
        // optional-mod skip-with-warning path in the importer.
        (203, 9003) => serde_json::json!({ "data": {
            "downloadUrl": format!("{}/cdn/optional.jar", m.base()),
            "fileName": "optional.jar",
        }}),
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    Json(json).into_response()
}

/// Build the mock server (and its fixture zips); returns its base URL.
async fn spawn_mock() -> (String, Arc<Mock>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let mock = Arc::new(Mock::default());
    mock.base.set(base.clone()).unwrap();

    let opts: zip::write::FileOptions<'_, ()> = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);

    // Forge modpack zip: manifest + overrides.
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    zip.start_file("manifest.json", opts).unwrap();
    zip.write_all(
        br#"{
        "manifestType": "modPack", "manifestVersion": 1,
        "name": "Adventure Wasteland", "version": "1.0.0", "author": "Tester",
        "files": [
            { "projectID": 201, "fileID": 9001, "required": true },
            { "projectID": 202, "fileID": 9002, "required": true },
            { "projectID": 203, "fileID": 9003, "required": false }
        ],
        "overrides": "overrides",
        "minecraft": { "version": "1.20.1", "modLoaders": [{ "id": "forge-47.2.0", "primary": true }] }
        }"#,
    )
    .unwrap();
    zip.start_file("overrides/config/game.cfg", opts).unwrap();
    zip.write_all(b"adventure=on").unwrap();
    zip.start_file("overrides/options.txt", opts).unwrap();
    zip.write_all(b"renderDistance:12").unwrap();
    mock.files
        .lock()
        .unwrap()
        .insert("adventure.zip".into(), zip.finish().unwrap().into_inner());

    // Fabric pack zip: modrinth.index.json with a direct download URL.
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let index = format!(
        r#"{{
        "game": "minecraft", "name": "Fabric Fields", "version": "0.3",
        "author": "Tester",
        "files": [
            {{ "path": "mods/fabric-first.jar", "hashes": {{}},
              "env": {{ "client": "required", "server": "required" }},
              "downloads": ["{base}/cdn/first.jar"], "fileSize": 11 }}
        ],
        "dependencies": {{ "fabric-loader": "0.15.0" }},
        "gameVersions": ["1.21.4", "Fabric"]
        }}"#
    );
    zip.start_file("modrinth.index.json", opts).unwrap();
    zip.write_all(index.as_bytes()).unwrap();
    mock.files
        .lock()
        .unwrap()
        .insert("fields.zip".into(), zip.finish().unwrap().into_inner());

    let mut files = mock.files.lock().unwrap();
    files.insert("alpha.jar".into(), b"alpha-mod-bytes".to_vec());
    files.insert("beta.jar".into(), b"beta-mod-bytes".to_vec());
    files.insert("first.jar".into(), b"first-bytes".to_vec());
    drop(files);

    let app = axum::Router::new()
        .route("/v1/mods/search", get(search))
        .route("/v1/mods/:id/files", get(mod_files))
        .route("/v1/mods/:id/files/:file/download", get(pack_download))
        .route("/v1/mods/:project/files/:file", get(file_info))
        .route("/cdn/:name", get(cdn))
        .with_state(mock.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, mock)
}

async fn spawn_server(data_dir: &std::path::Path, cf_base: String, key: Option<String>) -> String {
    let cfg = ServerConfig {
        addr: "127.0.0.1:0".into(),
        data_dir: data_dir.to_path_buf(),
        token: None,
        repo_dist: None,
        max_upload_mb: 8,
        curseforge_key: key,
        curseforge_url: cf_base,
    };
    let state = AppState::new(&cfg).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    tokio::spawn(async move { axum::serve(listener, build_router(state)).await.unwrap() });
    base
}

#[tokio::test]
async fn curseforge_search_files_and_add_build_the_pack() {
    let (cf, mock) = spawn_mock().await;
    let data = tempfile::tempdir().unwrap();
    let base = spawn_server(data.path(), format!("{cf}/v1"), Some(KEY.into())).await;

    let status: serde_json::Value = reqwest::Client::new()
        .get(format!("{base}/console/curseforge"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status["configured"], true);

    // Search maps console params onto the Core API; protocol-relative icons
    // get an https: prefix; the modpack class id is enforced.
    let list: serde_json::Value = reqwest::Client::new()
        .get(format!(
            "{base}/console/curseforge/search?q=adventure&sort=popularity"
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list["packs"].as_array().unwrap().len(), 2);
    assert_eq!(list["packs"][0]["id"], 1000);
    assert_eq!(list["packs"][0]["icon"], "https://cdn.example/icon1.png");
    assert_eq!(list["total"], 2);
    assert_eq!(mock.searches.lock().unwrap().last().unwrap(), "adventure");

    // Versions listed for the selected pack.
    let versions: serde_json::Value = reqwest::Client::new()
        .get(format!("{base}/console/curseforge/files?mod_id=1000"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(versions["versions"].as_array().unwrap().len(), 1);
    assert_eq!(versions["versions"][0]["id"], 5001);

    // Add: full import + build.
    let res = reqwest::Client::new()
        .post(format!("{base}/console/curseforge/add"))
        .json(&serde_json::json!({
            "mod_id": 1000, "file_id": 5001, "id": "adventure", "title": "Adventure Wasteland",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let built: serde_json::Value = res.json().await.unwrap();
    assert_eq!(built["built"], true);
    assert_eq!(built["mods_fetched"], 2);
    assert_eq!(
        built["warnings"].as_array().unwrap().len(),
        1,
        "optional mod skipped"
    );
    assert_eq!(built["pack_version"], "1.0.0");
    assert_eq!(built["game_version"], "1.20.1");
    assert_eq!(built["loader"], "forge-47.2.0");

    let src = data.path().join("packs/adventure/src");
    assert_eq!(
        std::fs::read(src.join("mods/alpha-mod-1.2.jar")).unwrap(),
        b"alpha-mod-bytes"
    );
    assert_eq!(
        std::fs::read(src.join("mods/beta mod 2.jar")).unwrap(),
        b"beta-mod-bytes"
    );
    // overrides merged into src/
    assert_eq!(
        std::fs::read(src.join("config/game.cfg")).unwrap(),
        b"adventure=on"
    );
    assert_eq!(
        std::fs::read(src.join("options.txt")).unwrap(),
        b"renderDistance:12"
    );
    let pack_json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(src.join("pack.json")).unwrap()).unwrap();
    assert_eq!(pack_json["title"], "Adventure Wasteland");
    assert_eq!(pack_json["game_version"], "1.20.1");
    assert_eq!(pack_json["version"], "1.0.0");

    // Registry shows it built; the public route serves it. pack.json stays out.
    let packs: serde_json::Value = reqwest::Client::new()
        .get(format!("{base}/console/packs"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let row = packs["packs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == "adventure")
        .unwrap();
    assert_eq!(row["status"], "ok");
    assert_eq!(row["pack_version"], "1.0.0");
    assert_eq!(row["file_count"], 4); // 2 mods + game.cfg + options.txt

    let public: serde_json::Value = reqwest::Client::new()
        .get(format!("{base}/packs"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(public["packs"]
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p["id"] == "adventure"));

    // Duplicate ids are refused before touching upstream again.
    let dup = reqwest::Client::new()
        .post(format!("{base}/console/curseforge/add"))
        .json(&serde_json::json!({ "mod_id": 1000, "file_id": 5001, "id": "adventure" }))
        .send()
        .await
        .unwrap();
    assert_eq!(dup.status(), 400);
}

#[tokio::test]
async fn curseforge_fabric_direct_downloads() {
    let (cf, _mock) = spawn_mock().await;
    let data = tempfile::tempdir().unwrap();
    let base = spawn_server(data.path(), format!("{cf}/v1"), Some(KEY.into())).await;

    let res = reqwest::Client::new()
        .post(format!("{base}/console/curseforge/add"))
        .json(&serde_json::json!({ "mod_id": 1100, "file_id": 5101, "id": "fields" }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let built: serde_json::Value = res.json().await.unwrap();
    assert_eq!(built["game_version"], "1.21.4");
    assert_eq!(built["pack_version"], "0.3");

    let src = data.path().join("packs/fields/src");
    assert_eq!(
        std::fs::read(src.join("mods/fabric-first.jar")).unwrap(),
        b"first-bytes"
    );
}

#[tokio::test]
async fn required_mod_failure_aborts_the_import() {
    let (cf, mock) = spawn_mock().await;
    // A required mod's blob is missing: the import must fail, not half-build.
    mock.files.lock().unwrap().remove("alpha.jar");
    let data = tempfile::tempdir().unwrap();
    let base = spawn_server(data.path(), format!("{cf}/v1"), Some(KEY.into())).await;

    let res = reqwest::Client::new()
        .post(format!("{base}/console/curseforge/add"))
        .json(&serde_json::json!({ "mod_id": 1000, "file_id": 5001, "id": "broken" }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 502);
    let packs: serde_json::Value = reqwest::Client::new()
        .get(format!("{base}/console/packs"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(packs["packs"]
        .as_array()
        .unwrap()
        .iter()
        .all(|p| p["id"] != "broken"));
}

#[tokio::test]
async fn browsing_without_a_key_is_refused() {
    let data = tempfile::tempdir().unwrap();
    let base = spawn_server(data.path(), "http://127.0.0.1:1/v1".into(), None).await;

    let res = reqwest::Client::new()
        .get(format!("{base}/console/curseforge/search?q=x"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 400);
    let body: serde_json::Value = res.json().await.unwrap();
    assert!(body["message"]
        .as_str()
        .unwrap()
        .contains("LAUNCHCRAFT_CURSEFORGE_KEY"));

    let status: serde_json::Value = reqwest::Client::new()
        .get(format!("{base}/console/curseforge"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status["configured"], false);
}
