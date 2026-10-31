//! Multi-provider modpack tests: one mock server plays CurseForge (`/v1`),
//! Modrinth (`/mr/v2`) and the legacy FTB CDN (`/ftb`), plus `/cdn` blobs.
//! Covers search/version listing for every provider and the full add →
//! registry → build pipeline in all three archive flavours: CurseForge
//! `manifest.json`, Modrinth `.mrpack`, and baked `minecraft/` trees —
//! including in-place updates (prune for manifest packs, wipe-replace for
//! baked ones).

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

/// The legacy FTB catalog. Real ones carry dozens of packs with HTML-blurb
/// descriptions; this keeps the same shape (entities, `;`-separated
/// oldVersions that repeat the current version).
const FTB_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<modpacks>
	<modpack author="The FTB Team" curseProjectId="0" description="A &lt;b&gt;farm&lt;/b&gt; pack &amp; friends" dir="FTBFarm" image="FTBFarmSplash.png" logo="FTBFarmIcon.png" mcVersion="1.12.2" minJRE="1.8" mods="" name="FTB Farm" oldVersions="1.1.0;1.0.0" repoVersion="1_1_0" serverPack="FTBFarmServer.zip" url="FTBFarm.zip" version="1.1.0"/>
</modpacks>"#;

/// A stand-in for all three upstreams at once. `base` is the mock's own URL,
/// filled after binding so download links point back at this very server.
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

// ------------------------------------------------------------------ blobs

async fn cdn(State(m): State<Arc<Mock>>, Path(name): Path<String>) -> impl IntoResponse {
    let files = m.files.lock().unwrap();
    match files.get(&name) {
        Some(bytes) => (StatusCode::OK, bytes.clone()),
        None => (StatusCode::NOT_FOUND, Vec::new()),
    }
}

// ---------------------------------------------------------------- curseforge

async fn cf_search(
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
    Json(serde_json::json!({
        "data": [
            {
                "id": 1000, "name": "Adventure Wasteland",
                "summary": "You are stranded in a Fallout Wasteland, But, You ar...",
                "imageUrl": { "url": "//cdn.example/icon1.png" },
                "downloadCount": 123456,
            },
        ],
        "pagination": { "index": 0, "pageSize": 20, "resultCount": 1, "totalCount": 1 },
    }))
    .into_response()
}

async fn cf_files(
    State(m): State<Arc<Mock>>,
    headers: HeaderMap,
    Path(id): Path<u64>,
) -> impl IntoResponse {
    if let Err(e) = m.check_key(&headers) {
        return e.into_response();
    }
    let json = match id {
        1000 => serde_json::json!({ "data": [
            {
                "id": 5001, "displayName": "Adventure 1.0.0", "fileName": "adventure-1.0.0.zip",
                "fileLength": 2048, "releaseDate": "2026-05-01T00:00:00Z",
                "gameVersions": ["1.20.1", "Forge"],
            },
            {
                "id": 5002, "displayName": "Adventure 2.0.0", "fileName": "adventure-2.0.0.zip",
                "fileLength": 2560, "releaseDate": "2026-09-01T00:00:00Z",
                "gameVersions": ["1.20.1", "Forge"],
            },
        ] }),
        _ => serde_json::json!({ "data": [] }),
    };
    Json(json).into_response()
}

async fn cf_pack_download(
    State(m): State<Arc<Mock>>,
    headers: HeaderMap,
    Path((id, file)): Path<(u64, u64)>,
) -> impl IntoResponse {
    if let Err(e) = m.check_key(&headers) {
        return e.into_response();
    }
    let cdn_name = match (id, file) {
        (1000, 5001) => "adventure.zip",
        (1000, 5002) => "adventure2.zip",
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    Json(serde_json::json!({ "data": {
        "cdnUrl": format!("{}/cdn/{cdn_name}", m.base()),
        "fileName": cdn_name,
    }}))
    .into_response()
}

async fn cf_file_info(
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

// ----------------------------------------------------------------- modrinth

async fn mr_search(
    State(m): State<Arc<Mock>>,
    Query(q): Query<BTreeMap<String, String>>,
) -> impl IntoResponse {
    m.searches.lock().unwrap().push(format!(
        "mr:{}:{}",
        q.get("query").map(String::as_str).unwrap_or(""),
        q.get("facets").map(String::as_str).unwrap_or("")
    ));
    Json(serde_json::json!({
        "hits": [{
            "project_id": "abcd1234", "project_type": "modpack", "slug": "manor",
            "title": "Modrinth Manor", "description": "A tidy fabric pack.",
            "downloads": 4242, "icon_url": "https://cdn.example/mr.png",
        }],
        "total_hits": 1,
    }))
    .into_response()
}

/// Version listing; deliberately oldest first so the server's sort matters.
async fn mr_versions(State(m): State<Arc<Mock>>, Path(slug): Path<String>) -> impl IntoResponse {
    if slug != "manor" {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "project not found" })),
        )
            .into_response();
    }
    let base = m.base();
    Json(serde_json::json!([
        {
            "id": "V1", "name": "Manor 1.0", "version_number": "1.0",
            "date_published": "2026-05-01T00:00:00Z",
            "game_versions": ["1.21.1", "fabric"], "loaders": ["fabric"],
            "files": [{ "url": format!("{base}/cdn/manor1.mrpack"),
                        "filename": "manor-1.mrpack", "primary": true, "size": 10 }],
        },
        {
            "id": "V2", "name": "Manor 2.0", "version_number": "2.0",
            "date_published": "2026-09-01T00:00:00Z",
            "game_versions": ["1.21.1", "fabric"], "loaders": ["fabric"],
            "files": [{ "url": format!("{base}/cdn/manor2.mrpack"),
                        "filename": "manor-2.mrpack", "primary": true, "size": 12 }],
        },
    ]))
    .into_response()
}

async fn mr_version_file(State(m): State<Arc<Mock>>, Path(id): Path<String>) -> impl IntoResponse {
    let name = match id.as_str() {
        "V1" => "manor1.mrpack",
        "V2" => "manor2.mrpack",
        _ => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "version not found" })),
            )
                .into_response()
        }
    };
    Json(serde_json::json!({
        "id": id, "name": format!("Manor {id}"),
        "files": [{ "url": format!("{}/cdn/{name}", m.base()),
                    "filename": name, "primary": true, "size": 10 }],
    }))
    .into_response()
}

// ---------------------------------------------------------------------- ftb

async fn ftb_static(Path(page): Path<String>) -> impl IntoResponse {
    let xml = match page.as_str() {
        "modpacks.xml" => FTB_XML,
        "thirdparty.xml" => "<modpacks></modpacks>",
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    (
        [(axum::http::header::CONTENT_TYPE, "application/xml")],
        xml.to_string(),
    )
        .into_response()
}

async fn ftb_download(
    State(m): State<Arc<Mock>>,
    Path((dir, version, file)): Path<(String, String, String)>,
) -> impl IntoResponse {
    let key = format!("{dir}/{version}/{file}");
    let files = m.files.lock().unwrap();
    match files.get(&key) {
        Some(bytes) => (StatusCode::OK, bytes.clone()).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

// ----------------------------------------------------------------- fixtures

fn zip_build(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let opts: zip::write::FileOptions<'_, ()> = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for (name, bytes) in entries {
        zip.start_file(*name, opts).unwrap();
        zip.write_all(bytes).unwrap();
    }
    zip.finish().unwrap().into_inner()
}

/// One `.mrpack`: `modrinth.index.json` with direct download URLs plus a
/// small override. `files` maps the mod path to its cdn blob name.
fn mrpack(base: &str, version_id: &str, files: &[(&str, &str, &str)]) -> Vec<u8> {
    let entries: Vec<String> = files
        .iter()
        .map(|(path, blob, env)| {
            format!(
                r#"{{ "path": "{path}", "hashes": {{}}, "env": {{ "client": "{env}", "server": "required" }},
                    "downloads": ["{base}/cdn/{blob}"], "fileSize": 9 }}"#
            )
        })
        .collect();
    let index = format!(
        r#"{{ "game": "minecraft", "formatVersion": 1, "versionId": "{version_id}",
            "name": "Modrinth Manor", "author": "Tester",
            "files": [{}],
            "dependencies": {{ "fabric-loader": "0.15.0" }},
            "gameVersions": ["1.21.1", "fabric"] }}"#,
        entries.join(",")
    );
    zip_build(&[
        ("modrinth.index.json", index.as_bytes()),
        (
            "overrides/config/manor.cfg",
            format!("manor={version_id}").as_bytes(),
        ),
    ])
}

/// Build the mock server and every fixture zip; returns its base URL.
async fn spawn_mock() -> (String, Arc<Mock>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let mock = Arc::new(Mock::default());
    mock.base.set(base.clone()).unwrap();

    // Forge modpack zip v1: manifest + overrides.
    let adventure1 = zip_build(&[
        (
            "manifest.json",
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
        ),
        ("overrides/config/game.cfg", b"adventure=on".as_slice()),
        ("overrides/options.txt", b"renderDistance:12".as_slice()),
    ]);
    // v2 of the same project: beta mod removed, changed config, no options.txt.
    let adventure2 = zip_build(&[
        (
            "manifest.json",
            br#"{
            "manifestType": "modPack", "manifestVersion": 1,
            "name": "Adventure Wasteland", "version": "2.0.0", "author": "Tester",
            "files": [
                { "projectID": 201, "fileID": 9001, "required": true }
            ],
            "overrides": "overrides",
            "minecraft": { "version": "1.20.1", "modLoaders": [{ "id": "forge-47.2.0", "primary": true }] }
            }"#,
        ),
        ("overrides/config/game.cfg", b"adventure=v2".as_slice()),
    ]);

    // Modrinth mrpacks: v1 ships one mod + one optional it can't deliver;
    // v2 swaps in a second mod.
    let manor1 = mrpack(
        &base,
        "1.0",
        &[
            ("mods/mr-first.jar", "mr-first.jar", "required"),
            ("mods/mr-opt.jar", "mr-missing.jar", "unsupported"),
        ],
    );
    let manor2 = mrpack(
        &base,
        "2.0",
        &[
            ("mods/mr-first.jar", "mr-first.jar", "required"),
            ("mods/mr-third.jar", "mr-third.jar", "required"),
        ],
    );

    // Baked FTB trees: v1 carries a stale jar that v2 drops.
    let ftb1 = zip_build(&[
        ("minecraft/mods/ftb-old.jar", b"old-mod".as_slice()),
        ("minecraft/mods/stale-extra.jar", b"stale".as_slice()),
        ("minecraft/config/f.cfg", b"farm=v1".as_slice()),
        ("minecraft/version.json", b"1.0.0".as_slice()),
    ]);
    let ftb2 = zip_build(&[
        ("minecraft/mods/ftb-new.jar", b"new-mod".as_slice()),
        ("minecraft/config/f.cfg", b"farm=v2".as_slice()),
        ("minecraft/version.json", b"1.1.0".as_slice()),
    ]);

    let mut files = mock.files.lock().unwrap();
    files.insert("adventure.zip".into(), adventure1);
    files.insert("adventure2.zip".into(), adventure2);
    files.insert("manor1.mrpack".into(), manor1);
    files.insert("manor2.mrpack".into(), manor2);
    files.insert("FTBFarm/1_0_0/FTBFarm.zip".into(), ftb1);
    files.insert("FTBFarm/1_1_0/FTBFarm.zip".into(), ftb2);
    files.insert("alpha.jar".into(), b"alpha-mod-bytes".to_vec());
    files.insert("beta.jar".into(), b"beta-mod-bytes".to_vec());
    files.insert("mr-first.jar".into(), b"mr-first-bytes".to_vec());
    files.insert("mr-third.jar".into(), b"mr-third-bytes".to_vec());
    drop(files);

    let app = axum::Router::new()
        .route("/v1/mods/search", get(cf_search))
        .route("/v1/mods/:id/files", get(cf_files))
        .route("/v1/mods/:id/files/:file/download", get(cf_pack_download))
        .route("/v1/mods/:project/files/:file", get(cf_file_info))
        .route("/mr/v2/search", get(mr_search))
        .route("/mr/v2/project/:slug/version", get(mr_versions))
        .route("/mr/v2/version/:id", get(mr_version_file))
        .route("/ftb/static/:page", get(ftb_static))
        .route("/ftb/modpacks/:dir/:version/:file", get(ftb_download))
        .route("/cdn/:name", get(cdn))
        .with_state(mock.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, mock)
}

async fn spawn_server(
    data_dir: &std::path::Path,
    cf_base: String,
    key: Option<String>,
    mock_base: &str,
) -> String {
    let cfg = ServerConfig {
        addr: "127.0.0.1:0".into(),
        data_dir: data_dir.to_path_buf(),
        token: None,
        repo_dist: None,
        max_upload_mb: 8,
        curseforge_key: key,
        curseforge_url: cf_base,
        modrinth_url: format!("{mock_base}/mr/v2"),
        ftb_url: format!("{mock_base}/ftb"),
    };
    let state = AppState::new(&cfg).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    tokio::spawn(async move { axum::serve(listener, build_router(state)).await.unwrap() });
    base
}

// -------------------------------------------------------------------- tests

#[tokio::test]
async fn providers_are_listed_with_their_configuration() {
    let (mock, _m) = spawn_mock().await;
    let data = tempfile::tempdir().unwrap();
    let base = spawn_server(data.path(), format!("{mock}/v1"), Some(KEY.into()), &mock).await;
    let client = reqwest::Client::new();

    let list: serde_json::Value = client
        .get(format!("{base}/console/modpacks"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ids: Vec<&str> = list["providers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["curseforge", "modrinth", "ftb"]);
    for p in list["providers"].as_array().unwrap() {
        assert_eq!(p["configured"], true, "{} configured", p["id"]);
    }

    // Without a CurseForge key only that provider drops out.
    let data2 = tempfile::tempdir().unwrap();
    let base2 = spawn_server(data2.path(), format!("{mock}/v1"), None, &mock).await;
    let list: serde_json::Value = reqwest::Client::new()
        .get(format!("{base2}/console/modpacks"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    for p in list["providers"].as_array().unwrap() {
        let want = p["id"] != "curseforge";
        assert_eq!(p["configured"], want);
    }
    // And asking CurseForge without a key explains the env var to set.
    let res = reqwest::Client::new()
        .get(format!(
            "{base2}/console/modpacks/search?provider=curseforge&q=x"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 400);
    let body: serde_json::Value = res.json().await.unwrap();
    assert!(body["message"]
        .as_str()
        .unwrap()
        .contains("LAUNCHCRAFT_CURSEFORGE_KEY"));
    // Keyless providers still browse.
    let res = reqwest::Client::new()
        .get(format!("{base2}/console/modpacks/search?provider=ftb"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
}

#[tokio::test]
async fn unknown_provider_is_refused() {
    let (mock, _m) = spawn_mock().await;
    let data = tempfile::tempdir().unwrap();
    let base = spawn_server(data.path(), format!("{mock}/v1"), Some(KEY.into()), &mock).await;
    let res = reqwest::Client::new()
        .get(format!("{base}/console/modpacks/search?provider=technic"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 400);
}

#[tokio::test]
async fn curseforge_search_versions_and_add_build_the_pack() {
    let (mock, mockstate) = spawn_mock().await;
    let data = tempfile::tempdir().unwrap();
    let base = spawn_server(data.path(), format!("{mock}/v1"), Some(KEY.into()), &mock).await;
    let client = reqwest::Client::new();

    // Search maps console params onto the Core API; protocol-relative icons
    // get an https: prefix; the modpack class id is enforced by the mock.
    let list: serde_json::Value = client
        .get(format!(
            "{base}/console/modpacks/search?provider=curseforge&q=adventure&sort=popularity"
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list["packs"].as_array().unwrap().len(), 1);
    assert_eq!(list["packs"][0]["id"], "1000");
    assert_eq!(list["packs"][0]["icon"], "https://cdn.example/icon1.png");
    assert_eq!(list["total"], 1);
    assert_eq!(
        mockstate.searches.lock().unwrap().last().unwrap(),
        "adventure"
    );

    // Versions listed for the selected pack, newest first (5002 > 5001).
    let versions: serde_json::Value = client
        .get(format!(
            "{base}/console/modpacks/versions?provider=curseforge&project=1000"
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(versions["versions"].as_array().unwrap().len(), 2);
    assert_eq!(versions["versions"][0]["id"], "5002");

    // Add: full import + build.
    let res = client
        .post(format!("{base}/console/modpacks/add"))
        .json(&serde_json::json!({
            "provider": "curseforge", "project": "1000", "file": "5001",
            "id": "adventure", "title": "Adventure Wasteland",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let built: serde_json::Value = res.json().await.unwrap();
    assert_eq!(built["built"], true);
    assert_eq!(built["provider"], "curseforge");
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

    // The registry row keeps provider provenance for the update button.
    let packs: serde_json::Value = client
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
    assert_eq!(row["source"]["provider"], "curseforge");
    assert_eq!(row["source"]["project"], "1000");
    assert_eq!(row["source"]["file"], "5001");

    // Duplicate ids are refused before touching upstream again.
    let dup = client
        .post(format!("{base}/console/modpacks/add"))
        .json(&serde_json::json!({
            "provider": "curseforge", "project": "1000", "file": "5001", "id": "adventure",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(dup.status(), 400);
}

#[tokio::test]
async fn curseforge_update_prunes_dropped_mods() {
    let (mock, _m) = spawn_mock().await;
    let data = tempfile::tempdir().unwrap();
    let base = spawn_server(data.path(), format!("{mock}/v1"), Some(KEY.into()), &mock).await;
    let client = reqwest::Client::new();

    client
        .post(format!("{base}/console/modpacks/add"))
        .json(&serde_json::json!({
            "provider": "curseforge", "project": "1000", "file": "5001",
            "id": "adventure", "title": "Adventure Wasteland",
        }))
        .send()
        .await
        .unwrap();
    let src = data.path().join("packs/adventure/src");
    assert!(src.join("mods/beta mod 2.jar").is_file());

    // Asking explicitly for the file already installed is a no-op.
    let res: serde_json::Value = client
        .post(format!("{base}/console/modpacks/update/adventure"))
        .json(&serde_json::json!({ "file": "5001" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(res["updated"], false);

    // Default update: newest file for the project (5002 = v2.0.0), beta pruned.
    let res = client
        .post(format!("{base}/console/modpacks/update/adventure"))
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200, "update failed: {:?}", res.text().await);
    let r: serde_json::Value = res.json().await.unwrap();
    assert_eq!(r["updated"], true);
    assert_eq!(r["from_file"], "5001");
    assert_eq!(r["file"], "5002");
    assert_eq!(r["pack_version"], "2.0.0");
    assert_eq!(r["pruned"], 1, "the beta mod dropped by v2 must be pruned");

    // The tree is now v2; overrides re-merged; the update kept registry title.
    assert!(!src.join("mods/beta mod 2.jar").exists());
    assert_eq!(
        std::fs::read(src.join("config/game.cfg")).unwrap(),
        b"adventure=v2"
    );
    let registry: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(data.path().join("packs.json")).unwrap())
            .unwrap();
    let entry = registry["packs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == "adventure")
        .unwrap();
    assert_eq!(entry["source"]["file"], "5002");
    assert_eq!(entry["title"], "Adventure Wasteland");

    // Again with no argument: already at the newest file.
    let res: serde_json::Value = client
        .post(format!("{base}/console/modpacks/update/adventure"))
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(res["updated"], false);
    assert_eq!(res["file"], "5002");
}

#[tokio::test]
async fn modrinth_add_builds_mrpack_and_update_takes_the_newest() {
    let (mock, mockstate) = spawn_mock().await;
    let data = tempfile::tempdir().unwrap();
    let base = spawn_server(data.path(), format!("{mock}/v1"), Some(KEY.into()), &mock).await;
    let client = reqwest::Client::new();

    // Search is scoped to modpacks by facets.
    let list: serde_json::Value = client
        .get(format!(
            "{base}/console/modpacks/search?provider=modrinth&q=manor"
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list["packs"][0]["id"], "manor");
    assert_eq!(list["packs"][0]["downloads"], 4242);
    assert_eq!(list["total"], 1);
    assert!(mockstate
        .searches
        .lock()
        .unwrap()
        .last()
        .unwrap()
        .contains("project_type:modpack"));

    // Versions newest-first despite the mock serving them oldest-first.
    let versions: serde_json::Value = client
        .get(format!(
            "{base}/console/modpacks/versions?provider=modrinth&project=manor"
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(versions["versions"][0]["id"], "V2");

    // Add v1: direct-download files + optional skip + override merge.
    let res = client
        .post(format!("{base}/console/modpacks/add"))
        .json(&serde_json::json!({
            "provider": "modrinth", "project": "manor", "file": "V1", "id": "manor",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200, "{:?}", res.text().await);
    let built: serde_json::Value = res.json().await.unwrap();
    assert_eq!(built["mods_fetched"], 1);
    assert_eq!(
        built["warnings"].as_array().unwrap().len(),
        1,
        "optional skip"
    );
    assert_eq!(built["game_version"], "1.21.1");
    assert_eq!(built["pack_version"], "1.0");

    let src = data.path().join("packs/manor/src");
    assert_eq!(
        std::fs::read(src.join("mods/mr-first.jar")).unwrap(),
        b"mr-first-bytes"
    );
    assert_eq!(
        std::fs::read(src.join("config/manor.cfg")).unwrap(),
        b"manor=1.0"
    );

    // Update with no target: the mock's newest (V2) replaces the mod set.
    let res = client
        .post(format!("{base}/console/modpacks/update/manor"))
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200, "{:?}", res.text().await);
    let r: serde_json::Value = res.json().await.unwrap();
    assert_eq!(r["updated"], true);
    assert_eq!(r["from_file"], "V1");
    assert_eq!(r["file"], "V2");
    assert_eq!(r["pack_version"], "2.0");

    assert!(src.join("mods/mr-third.jar").is_file());
    assert_eq!(
        std::fs::read(src.join("config/manor.cfg")).unwrap(),
        b"manor=2.0"
    );
    let packs: serde_json::Value = client
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
        .find(|p| p["id"] == "manor")
        .unwrap();
    assert_eq!(row["source"]["provider"], "modrinth");
    assert_eq!(row["source"]["file"], "V2");
}

#[tokio::test]
async fn ftb_baked_add_and_update_replaces_the_workspace() {
    let (mock, _m) = spawn_mock().await;
    let data = tempfile::tempdir().unwrap();
    let base = spawn_server(data.path(), format!("{mock}/v1"), Some(KEY.into()), &mock).await;
    let client = reqwest::Client::new();

    // The catalog search filters case-insensitively on name/description.
    let list: serde_json::Value = client
        .get(format!(
            "{base}/console/modpacks/search?provider=ftb&q=farm"
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list["packs"][0]["id"], "FTBFarm");
    assert_eq!(list["packs"][0]["name"], "FTB Farm");
    assert_eq!(list["packs"][0]["description"], "A farm pack & friends");
    assert_eq!(
        list["packs"][0]["icon"],
        format!("{}/ftb/static/FTBFarmIcon.png", mock)
    );
    assert_eq!(list["total"], 1);
    // A game version the catalog does not have filters everything out.
    let list: serde_json::Value = client
        .get(format!(
            "{base}/console/modpacks/search?provider=ftb&q=farm&game_version=1.7.10"
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list["packs"].as_array().unwrap().len(), 0);

    // Versions: current first, oldVersions deduped behind it.
    let versions: serde_json::Value = client
        .get(format!(
            "{base}/console/modpacks/versions?provider=ftb&project=FTBFarm"
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ids: Vec<&str> = versions["versions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["1.1.0", "1.0.0"]);

    // Add the old version: the baked minecraft/ tree becomes src/ directly,
    // metadata comes from the catalog (the archive has no manifest).
    let res = client
        .post(format!("{base}/console/modpacks/add"))
        .json(&serde_json::json!({
            "provider": "ftb", "project": "FTBFarm", "file": "1.0.0", "id": "farm",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200, "{:?}", res.text().await);
    let built: serde_json::Value = res.json().await.unwrap();
    assert_eq!(built["mods_fetched"], 4, "every baked file lands");
    assert_eq!(built["game_version"], "1.12.2");
    assert_eq!(built["pack_version"], "1.0.0");

    let src = data.path().join("packs/farm/src");
    assert_eq!(
        std::fs::read(src.join("mods/ftb-old.jar")).unwrap(),
        b"old-mod"
    );
    assert_eq!(
        std::fs::read(src.join("mods/stale-extra.jar")).unwrap(),
        b"stale"
    );
    assert_eq!(std::fs::read(src.join("config/f.cfg")).unwrap(), b"farm=v1");
    let pack_json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(src.join("pack.json")).unwrap()).unwrap();
    assert_eq!(pack_json["title"], "FTB Farm");
    assert_eq!(pack_json["game_version"], "1.12.2");

    // Update to the catalog's current version: the workspace is wiped and
    // replaced, so the stale extra jar cannot linger.
    let res = client
        .post(format!("{base}/console/modpacks/update/farm"))
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200, "{:?}", res.text().await);
    let r: serde_json::Value = res.json().await.unwrap();
    assert_eq!(r["updated"], true);
    assert_eq!(r["from_file"], "1.0.0");
    assert_eq!(r["file"], "1.1.0");
    assert_eq!(r["pack_version"], "1.1.0");

    assert_eq!(
        std::fs::read(src.join("mods/ftb-new.jar")).unwrap(),
        b"new-mod"
    );
    assert!(!src.join("mods/ftb-old.jar").exists());
    assert!(!src.join("mods/stale-extra.jar").exists());
    assert_eq!(std::fs::read(src.join("config/f.cfg")).unwrap(), b"farm=v2");
    let packs: serde_json::Value = client
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
        .find(|p| p["id"] == "farm")
        .unwrap();
    assert_eq!(row["source"]["provider"], "ftb");
    assert_eq!(row["source"]["file"], "1.1.0");
    assert_eq!(row["file_count"], 3); // ftb-new.jar + f.cfg + version.json
}

#[tokio::test]
async fn required_mod_failure_aborts_the_import() {
    let (mock, mockstate) = spawn_mock().await;
    // A required mod's blob is missing: the import must fail, not half-build.
    mockstate.files.lock().unwrap().remove("alpha.jar");
    let data = tempfile::tempdir().unwrap();
    let base = spawn_server(data.path(), format!("{mock}/v1"), Some(KEY.into()), &mock).await;

    let res = reqwest::Client::new()
        .post(format!("{base}/console/modpacks/add"))
        .json(&serde_json::json!({
            "provider": "curseforge", "project": "1000", "file": "5001", "id": "broken",
        }))
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
async fn update_requires_provenance() {
    let (mock, _m) = spawn_mock().await;
    let data = tempfile::tempdir().unwrap();
    let base = spawn_server(data.path(), format!("{mock}/v1"), Some(KEY.into()), &mock).await;
    let client = reqwest::Client::new();

    // A hand-made pack has no provenance → 400; an unknown id → 404.
    client
        .post(format!("{base}/console/packs/new"))
        .json(&serde_json::json!({ "id": "manual", "title": "Manual" }))
        .send()
        .await
        .unwrap();
    let res = client
        .post(format!("{base}/console/modpacks/update/manual"))
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 400);
    let res = client
        .post(format!("{base}/console/modpacks/update/ghost"))
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 404);
}
