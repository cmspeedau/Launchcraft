//! Pack Manager tests: New Pack scaffolding, Add Existing registration,
//! Modify, in-process Build, Check integrity verification, and Remove.

use std::path::{Path, PathBuf};

use launchcraft_server::{build_router, AppState, ServerConfig};

async fn spawn(cfg: ServerConfig) -> String {
    let state = AppState::new(&cfg).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, build_router(state)).await.unwrap();
    });
    format!("http://127.0.0.1:{port}")
}

fn cfg_for(data_dir: &Path) -> ServerConfig {
    ServerConfig {
        addr: "127.0.0.1:0".into(),
        data_dir: data_dir.to_path_buf(),
        token: None,
        repo_dist: None,
        max_upload_mb: 8,
    }
}

async fn post_json(base: &str, path: &str, body: serde_json::Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{base}{path}"))
        .json(&body)
        .send()
        .await
        .unwrap()
}

async fn list_packs(base: &str) -> serde_json::Value {
    reqwest::Client::new()
        .get(format!("{base}/console/packs"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

fn find<'a>(packs: &'a serde_json::Value, id: &str) -> &'a serde_json::Value {
    packs["packs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == serde_json::json!(id))
        .unwrap_or_else(|| panic!("pack {id} missing in {packs}"))
}

#[tokio::test]
async fn new_pack_scaffolds_workspace_and_registers() {
    let data = tempfile::tempdir().unwrap();
    let base = spawn(cfg_for(data.path())).await;

    let res = post_json(
        &base,
        "/console/packs/new",
        serde_json::json!({
            "id": "alice", "title": "Alice 1.7", "game_version": "1.7.10",
        }),
    )
    .await;
    assert_eq!(res.status(), 201);

    let src = data.path().join("packs/alice/src");
    assert!(src.join("mods").is_dir() && src.join("config").is_dir());
    let pack_json = std::fs::read_to_string(src.join("pack.json")).unwrap();
    assert!(pack_json.contains("\"alice\"") && pack_json.contains("1.7.10"));

    let packs = list_packs(&base).await;
    assert_eq!(find(&packs, "alice")["status"], "not-built");
    assert_eq!(find(&packs, "alice")["title"], "Alice 1.7");

    // Duplicate ids are refused.
    let dup = post_json(
        &base,
        "/console/packs/new",
        serde_json::json!({ "id": "alice", "title": "x" }),
    )
    .await;
    assert_eq!(dup.status(), 400);
    // And bad ids never touch the filesystem.
    let bad = post_json(
        &base,
        "/console/packs/new",
        serde_json::json!({ "id": "../evil", "title": "x" }),
    )
    .await;
    assert_eq!(bad.status(), 400);
    assert!(!data.path().join("packs").exists() || data.path().join("packs/alice").exists());
}

#[tokio::test]
async fn build_check_modify_remove_flow() {
    let data = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    let base = spawn(cfg_for(data.path())).await;

    post_json(
        &base,
        "/console/packs/new",
        serde_json::json!({ "id": "buildy", "title": "Buildy" }),
    )
    .await;
    let res = reqwest::Client::new()
        .put(format!("{base}/console/packs/buildy"))
        .json(&serde_json::json!({ "src": src.path().display().to_string() }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);

    std::fs::create_dir_all(src.path().join("mods")).unwrap();
    std::fs::write(src.path().join("mods/example.jar"), b"hello-modpack").unwrap();

    // Build in-process through the builder crate.
    let built: serde_json::Value = post_json(
        &base,
        "/console/packs/buildy/build",
        serde_json::json!({ "version": "2.0.0" }),
    )
    .await
    .json()
    .await
    .unwrap();
    assert_eq!(built["built"], true);
    assert_eq!(built["pack_version"], "2.0.0");
    assert_eq!(built["file_count"], 1);

    let packs = list_packs(&base).await;
    assert_eq!(find(&packs, "buildy")["status"], "ok");
    assert_eq!(find(&packs, "buildy")["pack_version"], "2.0.0");

    // Check: everything present and correctly hashed.
    let report: serde_json::Value =
        post_json(&base, "/console/packs/buildy/check", serde_json::json!({}))
            .await
            .json()
            .await
            .unwrap();
    assert_eq!(report["ok"], true);
    assert_eq!(report["files"], 1);

    // Break the dist: delete the CAS blob, then corrupt it.
    let dist = data.path().join("packs/buildy/dist");
    let manifest: launchcraft_core::manifest::PackManifest =
        serde_json::from_str(&std::fs::read_to_string(dist.join("manifest.json")).unwrap())
            .unwrap();
    let hash = &manifest.files["mods/example.jar"].hash;
    let blob = dist.join("cas/objects").join(&hash[..2]).join(hash);
    assert!(blob.is_file());
    std::fs::remove_file(&blob).unwrap();
    let report: serde_json::Value =
        post_json(&base, "/console/packs/buildy/check", serde_json::json!({}))
            .await
            .json()
            .await
            .unwrap();
    assert_eq!(report["ok"], false);
    assert_eq!(report["missing"], 1);

    std::fs::create_dir_all(blob.parent().unwrap()).unwrap();
    std::fs::write(&blob, b"not-a-zstd-frame").unwrap();
    let report: serde_json::Value =
        post_json(&base, "/console/packs/buildy/check", serde_json::json!({}))
            .await
            .json()
            .await
            .unwrap();
    assert_eq!(report["ok"], false);
    assert_eq!(report["hash_mismatch"], 1);

    // Modify: rename only.
    let res: serde_json::Value = reqwest::Client::new()
        .put(format!("{base}/console/packs/buildy"))
        .json(&serde_json::json!({ "title": "Buildy Renamed" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(res["title"], "Buildy Renamed");

    // Remove: registry row goes away, dist files stay.
    let res = reqwest::Client::new()
        .delete(format!("{base}/console/packs/buildy"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    assert!(list_packs(&base).await["packs"]
        .as_array()
        .unwrap()
        .is_empty());
    assert!(dist.join("manifest.json").is_file());
    // Unknown packs 404.
    let gone = reqwest::Client::new()
        .delete(format!("{base}/console/packs/buildy"))
        .send()
        .await
        .unwrap();
    assert_eq!(gone.status(), 404);
}

#[tokio::test]
async fn add_existing_requires_real_directories_and_parses_manifest() {
    let data = tempfile::tempdir().unwrap();
    let dist = tempfile::tempdir().unwrap();
    let base = spawn(cfg_for(data.path())).await;

    // dist must exist server-side.
    let missing = post_json(
        &base,
        "/console/packs",
        serde_json::json!({
            "id": "ghost", "title": "Ghost", "dist": "/no/such/dir-xyz",
        }),
    )
    .await;
    assert_eq!(missing.status(), 400);

    // A prebuilt dist registers and reads as ok.
    let mut files = std::collections::BTreeMap::new();
    files.insert(
        "mods/one.jar".to_string(),
        launchcraft_core::manifest::ManifestEntry {
            hash: "cd".repeat(32),
            size: 10,
            compressed_size: Some(5),
            user_override: false,
            executable: false,
        },
    );
    let pack = launchcraft_core::manifest::PackManifest {
        pack_id: "ext".into(),
        pack_version: "0.9.1".into(),
        game_version: "1.21.4".into(),
        loader: None,
        created_at: chrono::Utc::now(),
        files,
        user_overrides: vec![],
    };
    std::fs::write(
        dist.path().join("manifest.json"),
        serde_json::to_vec_pretty(&pack).unwrap(),
    )
    .unwrap();

    let res = post_json(
        &base,
        "/console/packs",
        serde_json::json!({
            "id": "ext", "title": "External Pack",
            "src": src_placeholder(dist.path()).display().to_string(),
            "dist": dist.path().display().to_string(),
        }),
    )
    .await;
    assert_eq!(res.status(), 201);

    let packs = list_packs(&base).await;
    let row = find(&packs, "ext");
    assert_eq!(row["status"], "ok");
    assert_eq!(row["pack_version"], "0.9.1");
    assert_eq!(row["game_version"], "1.21.4");
    assert_eq!(row["file_count"], 1);
}

fn src_placeholder(anywhere: &Path) -> PathBuf {
    // Any existing directory is fine for src validation purposes.
    anywhere.to_path_buf()
}
