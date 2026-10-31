//! End-to-end round-trips against a live server using the real
//! `launchcraft-backup` archiver and sync client.

use std::path::Path;

use launchcraft_backup::archiver::WorldArchiver;
use launchcraft_backup::client::BackupSyncClient;
use launchcraft_backup::inspector::WorldInspector;
use launchcraft_core::instance::{BackupArchiveInfo, BackupConfig};
use launchcraft_server::{build_router, AppState, ServerConfig};

struct TestServer {
    base: String,
    _data: tempfile::TempDir,
}

async fn spawn(cfg: ServerConfig) -> TestServer {
    let data = tempfile::tempdir().unwrap();
    let mut cfg = cfg;
    cfg.data_dir = data.path().to_path_buf();
    let state = AppState::new(&cfg).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, build_router(state)).await.unwrap();
    });
    TestServer {
        base: format!("http://127.0.0.1:{port}"),
        _data: data,
    }
}

fn open_config(endpoint: &str) -> BackupConfig {
    BackupConfig {
        server_backups_enabled: true,
        endpoint_url: Some(endpoint.to_string()),
        max_local_backups: 5,
        auto_backup_on_launch: false,
    }
}

/// Build an instance with one world and return (instance dir, fresh archive).
fn make_instance_with_backup(name: &str) -> (tempfile::TempDir, BackupArchiveInfo) {
    let dir = tempfile::tempdir().unwrap();
    let world = dir.path().join("minecraft").join("saves").join(name);
    std::fs::create_dir_all(&world).unwrap();
    std::fs::write(world.join("level.dat"), b"fake-nbt-payload").unwrap();
    std::fs::write(world.join("icon.png"), vec![7u8; 4096]).unwrap();
    let info = WorldArchiver::create_backup(dir.path(), name).unwrap();
    (dir, info)
}

#[tokio::test]
async fn client_backup_upload_list_download_roundtrip() {
    let server = spawn(default_cfg()).await;
    let (instance, archive) = make_instance_with_backup("TestWorld");
    let config = open_config(&format!("{}/api", server.base));

    // The client's real upload path against the real store.
    let client = BackupSyncClient::new();
    let info = client
        .upload_backup(instance.path(), &config, &archive.file_name, None)
        .await
        .expect("upload must succeed");
    assert_eq!(info.file_name, archive.file_name);
    assert!(info.is_remote);

    // Listing via the JSON API sees exactly the uploaded archive.
    let list: serde_json::Value = reqwest::Client::new()
        .get(format!("{}/api/backups", server.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let backups = list["backups"].as_array().unwrap();
    assert_eq!(backups.len(), 1);
    assert_eq!(backups[0]["file_name"], archive.file_name);
    assert_eq!(backups[0]["world_name"], "TestWorld");
    assert_eq!(
        backups[0]["checksum_blake3"],
        serde_json::json!(blake3_hex(instance.path(), &archive.file_name))
    );
    assert_eq!(list["server_backups_enabled"], true);

    // Download into a *fresh* instance and compare against the original zip.
    let fresh = tempfile::tempdir().unwrap();
    let got = client
        .download_backup(fresh.path(), &config, &archive.file_name, None)
        .await
        .expect("download must succeed");
    assert_eq!(got.size_bytes, archive.size_bytes);
    let original =
        std::fs::read(WorldInspector::get_backups_dir(instance.path()).join(&archive.file_name))
            .unwrap();
    let restored =
        std::fs::read(WorldInspector::get_backups_dir(fresh.path()).join(&archive.file_name))
            .unwrap();
    assert_eq!(original, restored);
}

#[tokio::test]
async fn upload_refused_while_policy_disabled() {
    let server = spawn(default_cfg()).await;
    let (instance, archive) = make_instance_with_backup("PolicyWorld");

    reqwest::Client::new()
        .post(format!("{}/console/policy", server.base))
        .json(&serde_json::json!({ "server_backups_enabled": false }))
        .send()
        .await
        .unwrap();

    let client = BackupSyncClient::new();
    let err = client
        .upload_backup(
            instance.path(),
            &open_config(&format!("{}/api", server.base)),
            &archive.file_name,
            None,
        )
        .await
        .expect_err("disabled policy must reject the upload");
    assert!(
        matches!(
            err,
            launchcraft_backup::BackupError::RemoteServer { status: 403, .. }
        ),
        "unexpected error: {err:?}"
    );
}

#[tokio::test]
async fn path_traversal_filenames_rejected() {
    let server = spawn(default_cfg()).await;
    for bad in ["..%2F..%2Fevil.zip", "%2Fetc%2Fpasswd.zip"] {
        let res = reqwest::Client::new()
            .post(format!("{}/api/backups/upload?filename={bad}", server.base))
            .header(reqwest::header::CONTENT_TYPE, "application/zip")
            .body(b"zip".to_vec())
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 400, "must reject {bad}");
    }
}

#[tokio::test]
async fn token_protects_endpoints_when_configured() {
    let mut cfg = default_cfg();
    cfg.token = Some("s3cret".into());
    let server = spawn(cfg).await;
    let (instance, archive) = make_instance_with_backup("LockedWorld");
    let config = open_config(&format!("{}/api", server.base));
    let client = BackupSyncClient::new();

    // No token: rejected.
    let err = client
        .upload_backup(instance.path(), &config, &archive.file_name, None)
        .await
        .expect_err("anonymous upload must fail");
    assert!(matches!(
        err,
        launchcraft_backup::BackupError::RemoteServer { status: 401, .. }
    ));

    // With token: accepted, and the console sees the archive.
    let info = client
        .upload_backup(instance.path(), &config, &archive.file_name, Some("s3cret"))
        .await
        .expect("authorized upload must succeed");
    let stats: serde_json::Value = reqwest::Client::new()
        .get(format!("{}/console/stats", server.base))
        .bearer_auth("s3cret")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(stats["archive_count"], 1);
    assert_eq!(stats["worlds"][0]["world"], "LockedWorld");
    assert_eq!(info.world_name, "LockedWorld");

    // Console HTML itself stays reachable without a token.
    let page = reqwest::Client::new()
        .get(format!("{}/", server.base))
        .send()
        .await
        .unwrap();
    assert!(page.text().await.unwrap().contains("Server Console"));
}

#[tokio::test]
async fn root_mounted_aliases_work() {
    let server = spawn(default_cfg()).await;
    let res = reqwest::Client::new()
        .post(format!(
            "{}/backups/upload?filename=Alias_2026.zip",
            server.base
        ))
        .header(reqwest::header::CONTENT_TYPE, "application/zip")
        .body(b"zstdzip".to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 201);
    let res = reqwest::Client::new()
        .get(format!(
            "{}/backups/download?filename=Alias_2026.zip",
            server.base
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(&res.bytes().await.unwrap()[..], b"zstdzip");
}

#[tokio::test]
async fn console_packs_seeds_registry_from_repo_dist() {
    // First boot with LAUNCHCRAFT_REPO_DIST built: the pack shows up registered.
    let dist = tempfile::tempdir().unwrap();
    let mut files = std::collections::BTreeMap::new();
    files.insert(
        "mods/example.jar".to_string(),
        launchcraft_core::manifest::ManifestEntry {
            hash: "ab".repeat(32),
            size: 1234,
            compressed_size: Some(900),
            user_override: false,
            executable: false,
        },
    );
    let pack = launchcraft_core::manifest::PackManifest {
        pack_id: "demo-skyblock".into(),
        pack_version: "1.0.0".into(),
        game_version: "1.20.4".into(),
        loader: Some("fabric".into()),
        created_at: chrono::Utc::now(),
        files,
        user_overrides: vec![],
    };
    std::fs::write(
        dist.path().join("manifest.json"),
        serde_json::to_vec_pretty(&pack).unwrap(),
    )
    .unwrap();

    let mut cfg = default_cfg();
    cfg.repo_dist = Some(dist.path().to_path_buf());
    let server = spawn(cfg).await;

    let got: serde_json::Value = reqwest::Client::new()
        .get(format!("{}/console/packs", server.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let packs = got["packs"].as_array().unwrap();
    assert_eq!(packs.len(), 1);
    assert_eq!(packs[0]["id"], "demo-skyblock");
    assert_eq!(packs[0]["status"], "ok");
    assert_eq!(packs[0]["file_count"], 1);
    assert_eq!(packs[0]["game_version"], "1.20.4");
}

fn default_cfg() -> ServerConfig {
    ServerConfig {
        addr: "127.0.0.1:0".into(),
        data_dir: Default::default(),
        token: None,
        repo_dist: None,
        max_upload_mb: 8,
        curseforge_key: None,
        curseforge_url: "https://api.curseforge.com/v1".into(),
        modrinth_url: "https://api.modrinth.com/v2".into(),
        ftb_url: "https://dist.creeper.host/FTB2".into(),
    }
}

fn blake3_hex(instance_dir: &Path, name: &str) -> String {
    let bytes = std::fs::read(WorldInspector::get_backups_dir(instance_dir).join(name)).unwrap();
    blake3::hash(&bytes).to_hex().to_string()
}
