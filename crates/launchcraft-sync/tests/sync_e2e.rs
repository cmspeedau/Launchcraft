//! End-to-end synchronization against a builder-layout pack repository.
//!
//! Serves `manifest.json` + sharded `cas/objects/<xx>/<hash>` blobs exactly
//! as `launchcraft-builder` writes them (and as the compose `repo-server`
//! publishes them) over a minimal HTTP server, then runs `SyncEngine` and
//! verifies deployment, deletions, and whitelist preservation.

use launchcraft_core::manifest::{ManifestEntry, PackManifest};
use launchcraft_sync::{CasStore, SyncEngine};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn spawn_repo_server(root: PathBuf) -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let root = root.clone();
            std::thread::spawn(move || handle_connection(stream, &root));
        }
    });
    port
}

fn handle_connection(mut stream: std::net::TcpStream, root: &Path) {
    let mut buf = [0u8; 4096];
    loop {
        let n = match stream.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        let request = String::from_utf8_lossy(&buf[..n]).to_string();
        let Some(path) = request.split_whitespace().nth(1).map(str::to_string) else {
            return;
        };
        let file_path = root.join(path.trim_start_matches('/'));
        match std::fs::read(&file_path) {
            Ok(body) => {
                let ctype = if path.ends_with(".json") {
                    "application/json"
                } else {
                    "application/octet-stream"
                };
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n",
                    body.len()
                );
                if stream.write_all(head.as_bytes()).is_err() || stream.write_all(&body).is_err() {
                    return;
                }
            }
            Err(_) => {
                let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
                return;
            }
        }
    }
}

fn write_cas_blob(repo: &Path, content: &[u8]) -> String {
    let hash = blake3::hash(content).to_hex().to_string();
    let blob = zstd::encode_all(content, 3).unwrap();
    let shard = repo.join("cas").join("objects").join(&hash[0..2]);
    std::fs::create_dir_all(&shard).unwrap();
    std::fs::write(shard.join(&hash), blob).unwrap();
    hash
}

#[tokio::test]
async fn synchronize_installs_builder_pack_and_prunes_extraneous_files() {
    let repo = tempfile::tempdir().unwrap();
    let game = tempfile::tempdir().unwrap();
    let cas = tempfile::tempdir().unwrap();

    // Build a builder-layout repository: zstd blobs, sharded objects,
    // path-keyed manifest.json.
    let mod_content = b"dummy mod content".to_vec();
    let cfg_content = b"key=value".to_vec();
    let mod_hash = write_cas_blob(repo.path(), &mod_content);
    let cfg_hash = write_cas_blob(repo.path(), &cfg_content);

    let mut files = BTreeMap::new();
    files.insert(
        "mods/foo.jar".to_string(),
        ManifestEntry {
            hash: mod_hash,
            size: mod_content.len() as u64,
            compressed_size: Some(42),
            user_override: false,
            executable: false,
        },
    );
    files.insert(
        "config/test.cfg".to_string(),
        ManifestEntry {
            hash: cfg_hash,
            size: cfg_content.len() as u64,
            compressed_size: Some(19),
            user_override: true,
            executable: false,
        },
    );
    let pack = PackManifest {
        pack_id: "e2e-pack".to_string(),
        pack_version: "1.0.0".to_string(),
        game_version: "1.20.4".to_string(),
        loader: None,
        created_at: chrono::Utc::now(),
        files,
        user_overrides: vec!["config/test.cfg".to_string()],
    };
    std::fs::write(
        repo.path().join("manifest.json"),
        serde_json::to_string(&pack).unwrap(),
    )
    .unwrap();

    // Pre-populate the game dir: one matching file, one extraneous file,
    // one whitelisted user file.
    std::fs::create_dir_all(game.path().join("config")).unwrap();
    std::fs::write(game.path().join("config/test.cfg"), &cfg_content).unwrap();
    std::fs::write(game.path().join("junk.txt"), b"leftover").unwrap();
    std::fs::create_dir_all(game.path().join("saves/World1")).unwrap();
    std::fs::write(game.path().join("saves/World1/level.dat"), b"save").unwrap();

    let port = spawn_repo_server(repo.path().to_path_buf());
    let engine = SyncEngine::new(Arc::new(CasStore::new(cas.path())));
    engine
        .synchronize(
            &format!("http://127.0.0.1:{port}"),
            game.path(),
            &pack,
            None,
        )
        .await
        .expect("synchronize failed");

    // Missing file installed with correct content.
    let installed = game.path().join("mods/foo.jar");
    assert!(installed.is_file(), "mods/foo.jar not materialized");
    assert_eq!(std::fs::read(&installed).unwrap(), mod_content);

    // Extraneous file pruned, whitelisted user file preserved.
    assert!(!game.path().join("junk.txt").exists(), "junk.txt survived");
    assert!(
        game.path().join("saves/World1/level.dat").exists(),
        "whitelisted save was deleted"
    );
}
