//! Multi-major Java provisioning tests. One mock plays the Adoptium assets +
//! package API, serving a hand-built `tar.gz` whose `bin/java` is a shell
//! script printing a real-looking version string — so the whole path
//! (assets lookup → download → sha256 verify → extract → probe → registry)
//! runs without touching the network or needing a JDK in the test image.
//!
//! The scenario that matters for a self-hosted server: a 1.7.10 pack (Java 8)
//! and a 1.20.5 pack (Java 21) exist at the same time and each get their own
//! managed JDK, side by side.

use std::io::Write as _;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use sha2::{Digest, Sha256};

use launchcraft_server::runtime_dist::{RuntimeEntry, RuntimeStatus, RuntimeStore};
use launchcraft_server::{build_router, AppState, ServerConfig};

/// Count package downloads so a second `ensure` can be proven to be free.
#[derive(Default)]
struct Mock {
    packages: std::sync::Mutex<std::collections::BTreeMap<String, Vec<u8>>>,
    downloads: AtomicUsize,
    base: std::sync::OnceLock<String>,
}

impl Mock {
    fn package(&self, name: &str) -> Option<Vec<u8>> {
        self.packages.lock().unwrap().get(name).cloned()
    }
}

/// A miniature "JDK": `jdk-<major>/bin/java` is an executable script whose
/// stderr looks like `java -version` output.
fn fake_jdk(major: u32) -> Vec<u8> {
    let root = format!("jdk-{major}");
    let script = format!(
        concat!(
            "#!/bin/sh\n",
            "echo 'openjdk version \"{}.0_422\"' >&2\n",
            "echo 'OpenJDK Runtime Environment (Temurin) (build 25.0.422+8)' >&2\n",
            "exit 0\n"
        ),
        if major >= 9 {
            major.to_string()
        } else {
            format!("1.{major}")
        }
    );
    tar_gz(&[
        (&format!("{root}/"), None, 0o755),
        (&format!("{root}/bin/"), None, 0o755),
        (&format!("{root}/bin/java"), Some(script.as_bytes()), 0o755),
        (
            &format!("{root}/release"),
            Some(b"JAVA_VERSION=\"x\"\n"),
            0o644,
        ),
    ])
}

fn tar_gz(entries: &[(&str, Option<&[u8]>, u32)]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for (name, body, mode) in entries {
        let mut header = tar::Header::new_gnu();
        header.set_mode(*mode);
        header.set_uid(0);
        header.set_gid(0);
        match body {
            Some(bytes) => {
                header.set_size(bytes.len() as u64);
                header.set_entry_type(tar::EntryType::Regular);
                builder.append_data(&mut header, name, *bytes).unwrap();
            }
            None => {
                header.set_size(0);
                header.set_entry_type(tar::EntryType::Directory);
                const EMPTY: &[u8] = &[];
                builder.append_data(&mut header, name, EMPTY).unwrap();
            }
        }
    }
    let raw = builder.into_inner().unwrap();
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(&raw).unwrap();
    encoder.finish().unwrap()
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>()
}

async fn assets(State(m): State<Arc<Mock>>, UrlPath(major): UrlPath<u32>) -> impl IntoResponse {
    let name = format!("jdk{major}.tar.gz");
    let Some(bytes) = m.package(&name) else {
        // Adoptium answers with an empty list for a major it does not publish.
        return (StatusCode::OK, "[]".to_string());
    };
    let link = format!("{}/pkg/{name}", m.base.get().unwrap());
    let body = serde_json::json!([
        {
            "release_name": format!("jdk-{major}+1"),
            "binary": {
                "package": {
                    "link": link,
                    "checksum": sha256_hex(&bytes),
                    "size": bytes.len() as u64,
                }
            },
            "binaries": [],
        }
    ])
    .to_string();
    (StatusCode::OK, body)
}

async fn package(State(m): State<Arc<Mock>>, UrlPath(name): UrlPath<String>) -> impl IntoResponse {
    m.downloads.fetch_add(1, Ordering::SeqCst);
    match m.package(&name) {
        Some(bytes) => (StatusCode::OK, bytes),
        None => (StatusCode::NOT_FOUND, Vec::new()),
    }
}

async fn spawn(data_dir: &Path) -> (String, Arc<Mock>) {
    let mock = Arc::new(Mock::default());
    for major in [8u32, 17, 21] {
        mock.packages
            .lock()
            .unwrap()
            .insert(format!("jdk{major}.tar.gz"), fake_jdk(major));
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    mock.base
        .set(format!(
            "http://127.0.0.1:{}",
            listener.local_addr().unwrap().port()
        ))
        .ok();
    let app = axum::Router::new()
        .route("/adoptium/v3/assets/latest/:major/hotspot", get(assets))
        .route("/pkg/:name", get(package))
        .with_state(mock.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let adoptium_base = mock.base.get().unwrap().clone();

    let cfg = ServerConfig {
        addr: "127.0.0.1:0".into(),
        data_dir: data_dir.to_path_buf(),
        max_upload_mb: 8,
        adoptium_url: format!("{adoptium_base}/adoptium/v3"),
        // Hermetic: never let a JDK installed on the test host satisfy a
        // requirement that the mock is supposed to serve.
        jres_host_java: false,
        ..Default::default()
    };
    let state = AppState::new(&cfg).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    tokio::spawn(async move { axum::serve(listener, build_router(state)).await.unwrap() });
    (base, mock)
}

async fn post(base: &str, body: serde_json::Value) -> (StatusCode, serde_json::Value) {
    let res = reqwest::Client::new()
        .post(format!("{base}/console/jres"))
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = res.status();
    (status, res.json().await.unwrap_or(serde_json::json!({})))
}

async fn list(base: &str) -> serde_json::Value {
    let res = reqwest::Client::new()
        .get(format!("{base}/console/jres"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    res.json().await.unwrap()
}

fn java_entry(list: &serde_json::Value, major: u32) -> &serde_json::Value {
    list["java"]
        .as_array()
        .unwrap()
        .iter()
        .find(|j| j["major"] == serde_json::json!(major))
        .unwrap_or_else(|| panic!("major {major} missing in {list}"))
}

/// Seed a runtime record the way a finished build would, so Java requirements
/// can be tested without a second Mojang mock.
fn seed_runtime(data_dir: &Path, id: &str, mc: &str, required_java: u32) {
    let store = RuntimeStore::new(data_dir);
    store.ensure_layout().unwrap();
    let mut registry = store.load_registry().unwrap();
    registry.upsert(RuntimeEntry {
        id: id.into(),
        mc_version: mc.into(),
        loader: None,
        loader_version: None,
        install_version: mc.into(),
        status: RuntimeStatus::Ready,
        assets: false,
        error: None,
        created_at: "2026-01-01T00:00:00Z".into(),
        updated_at: "2026-01-01T00:00:00Z".into(),
        file_count: 1,
        total_bytes: 1,
        stored_bytes: 1,
        required_java: Some(required_java),
    });
    store.save_registry(&registry).unwrap();
}

#[tokio::test]
async fn two_pack_eras_get_two_jdks() {
    let data = tempfile::tempdir().unwrap();
    let (base, mock) = spawn(data.path()).await;

    // A 1.7.10 pack and a 1.20.5 pack both exist on this server.
    seed_runtime(data.path(), "1.7.10--vanilla", "1.7.10", 8);
    seed_runtime(data.path(), "1.20.5--vanilla", "1.20.5", 21);

    let before = list(&base).await;
    assert_eq!(
        java_entry(&before, 8)["available"],
        serde_json::json!(false)
    );
    assert_eq!(
        java_entry(&before, 21)["available"],
        serde_json::json!(false)
    );
    assert_eq!(
        java_entry(&before, 8)["required_by"],
        serde_json::json!(["1.7.10--vanilla"])
    );
    assert_eq!(
        java_entry(&before, 21)["required_by"],
        serde_json::json!(["1.20.5--vanilla"])
    );

    // Provision both: separate directories, one download each.
    let (status, java8) = post(&base, serde_json::json!({ "major": 8 })).await;
    assert_eq!(status, 200);
    assert_eq!(java8["source"], "managed");
    assert_eq!(java8["version"], "1.8.0_422");
    let (status, java21) = post(&base, serde_json::json!({ "major": 21 })).await;
    assert_eq!(status, 200);
    assert_eq!(java21["version"], "21.0_422");

    assert!(data.path().join("jres/java-8/bin/java").is_file());
    assert!(data.path().join("jres/java-21/bin/java").is_file());
    assert_eq!(mock.downloads.load(Ordering::SeqCst), 2);

    let after = list(&base).await;
    assert_eq!(java_entry(&after, 8)["available"], serde_json::json!(true));
    assert_eq!(java_entry(&after, 21)["available"], serde_json::json!(true));
    // The listing reports both side by side, which is the whole point.
    assert_eq!(after["records"].as_array().unwrap().len(), 2);

    // Asking again must not re-download a JDK that is already there.
    let (status, again) = post(&base, serde_json::json!({ "major": 8 })).await;
    assert_eq!(status, 200);
    assert_eq!(again["path"], java8["path"]);
    assert_eq!(mock.downloads.load(Ordering::SeqCst), 2);

    // Deleting a JDK a runtime still needs is refused until forced, and
    // forcing only removes that major.
    let res = reqwest::Client::new()
        .delete(format!("{base}/console/jres/8"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 400);
    assert!(res.text().await.unwrap().contains("1.7.10--vanilla"));

    let res = reqwest::Client::new()
        .delete(format!("{base}/console/jres/8?force=1"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(
        res.json::<serde_json::Value>().await.unwrap()["deleted_tree"],
        true
    );
    assert!(!data.path().join("jres/java-8").exists());
    assert!(data.path().join("jres/java-21/bin/java").is_file());
}

#[tokio::test]
async fn java_6_and_7_point_at_the_override_instead_of_failing_silently() {
    let data = tempfile::tempdir().unwrap();
    let (base, _mock) = spawn(data.path()).await;

    let (status, body) = post(&base, serde_json::json!({ "major": 7 })).await;
    assert_eq!(status, 400);
    let message = body["message"].as_str().unwrap();
    assert!(message.contains("LAUNCHCRAFT_JAVA_7"), "message: {message}");

    let listed = list(&base).await;
    // Java 7 is still listed, so the console can explain why it needs an
    // override, but it is marked non-downloadable and nothing is recorded.
    let seven = java_entry(&listed, 7);
    assert_eq!(seven["available"], serde_json::json!(false));
    assert_eq!(seven["downloadable"], serde_json::json!(false));
    assert_eq!(
        java_entry(&listed, 8)["downloadable"],
        serde_json::json!(true)
    );
    assert_eq!(listed["records"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn unknown_major_and_bad_paths_are_rejected() {
    let data = tempfile::tempdir().unwrap();
    let (base, _mock) = spawn(data.path()).await;

    let res = reqwest::Client::new()
        .delete(format!("{base}/console/jres/not-a-number"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 400);
    let res = reqwest::Client::new()
        .delete(format!("{base}/console/jres/17"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 404);

    // An upstream with no release for the major fails as a gateway error and
    // records the failure instead of pretending the JDK exists.
    let (status, _) = post(&base, serde_json::json!({ "major": 99 })).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn provisioned_java_is_reused_by_a_second_server_instance() {
    let data = tempfile::tempdir().unwrap();
    let (base, mock) = spawn(data.path()).await;
    post(&base, serde_json::json!({ "major": 17 })).await;
    assert_eq!(mock.downloads.load(Ordering::SeqCst), 1);
    drop(base);

    // Restarting the container must not re-fetch: the managed tree is on the
    // data volume and the registry is rebuilt from it.
    let (base2, mock2) = spawn(data.path()).await;
    let listed = list(&base2).await;
    assert_eq!(
        java_entry(&listed, 17)["available"],
        serde_json::json!(true)
    );
    assert_eq!(java_entry(&listed, 17)["source"], "managed");
    assert_eq!(mock2.downloads.load(Ordering::SeqCst), 0);
    assert_eq!(mock.downloads.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn token_gates_the_java_endpoints() {
    let data = tempfile::tempdir().unwrap();
    let (base, mock) = spawn(data.path()).await;
    let cfg = ServerConfig {
        data_dir: data.path().to_path_buf(),
        token: Some("secret".into()),
        adoptium_url: mock
            .base
            .get()
            .map(|b| format!("{b}/adoptium/v3"))
            .unwrap_or_default(),
        ..Default::default()
    };
    let state = AppState::new(&cfg).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, build_router(state)).await.unwrap() });
    let auth_base = format!("http://127.0.0.1:{port}");

    let res = reqwest::Client::new()
        .get(format!("{auth_base}/console/jres"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 401);
    let res = reqwest::Client::new()
        .post(format!("{auth_base}/console/jres"))
        .json(&serde_json::json!({ "major": 8 }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 401);
    let res = reqwest::Client::new()
        .post(format!("{auth_base}/console/jres"))
        .header("authorization", "Bearer secret")
        .json(&serde_json::json!({ "major": 8 }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    // The first (tokenless) server is still up on `base`; both share the
    // data dir, so the JDK just provisioned is visible there too.
    assert_eq!(
        java_entry(&list(&base).await, 8)["available"],
        serde_json::json!(true)
    );
}
