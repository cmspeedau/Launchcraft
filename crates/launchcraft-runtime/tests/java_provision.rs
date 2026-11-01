//! Automatic Java provisioning against a mock of the Adoptium assets API:
//! builds a fake Temurin JDK tarball (and zip on Windows), serves it with
//! the real sha256, and verifies extraction, top-level stripping, exec bits,
//! cache skips, checksum enforcement and zip-slip rejection.

use std::io::Write as _;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::{routing, Router};
use serde_json::json;
use sha2::{Digest, Sha256};

use launchcraft_runtime::java::{AdoptiumClient, JavaInstallPhase, JavaRuntimeManager};
use launchcraft_runtime::{JavaDownloadProgress, JavaProgress, PlatformInfo};

const FAKE_JAVA: &[u8] = b"#!/bin/sh\necho temurin-fake\n";
const TOP: &str = "jdk-21.0.5+11";

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Overwrite a tar header's name field in-place (the tar crate refuses to
/// *create* hostile names through its safe API, so we tamper like a real
/// attacker would) and fix up the header checksum.
fn tamper_tar_name(bytes: &mut [u8], old: &str, new: &str) {
    let pos = find_subslice(bytes, old.as_bytes()).expect("tar entry name present");
    assert_eq!(pos % 512, 0, "name field starts a header block");
    let block = &mut bytes[pos..pos + 512];
    block[..old.len()].fill(0);
    block[..new.len()].copy_from_slice(new.as_bytes());
    block[148..156].fill(b' ');
    let sum: u32 = block.iter().map(|b| *b as u32).sum();
    let oct = format!("{:07o}\0", sum);
    block[148..156].copy_from_slice(oct.as_bytes());
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Fake JDK: single top-level dir, `bin/java` with an exec bit, one lib file.
/// `extra` injects an additional (possibly unsafe) entry name.
fn jdk_tar_gz(extra: Option<&str>) -> Vec<u8> {
    let lib_name = format!("{TOP}/lib/modules");
    let mut builder = tar::Builder::new(Vec::new());
    let mut h = tar::Header::new_gnu();
    h.set_path(format!("{TOP}/bin/java")).unwrap();
    h.set_size(FAKE_JAVA.len() as u64);
    h.set_mode(0o755);
    h.set_entry_type(tar::EntryType::Regular);
    h.set_mtime(0);
    h.set_cksum();
    builder.append(&h, FAKE_JAVA).unwrap();

    h.set_path(&lib_name).unwrap();
    h.set_size(7);
    h.set_mode(0o644);
    h.set_cksum();
    builder.append(&h, b"modules".as_slice()).unwrap();

    let mut tar_bytes = builder.into_inner().unwrap();
    if let Some(name) = extra {
        tamper_tar_name(&mut tar_bytes, &lib_name, name);
    }
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(&tar_bytes).unwrap();
    enc.finish().unwrap()
}

fn jdk_zip(extra: Option<&str>) -> Vec<u8> {
    let buf = std::io::Cursor::new(Vec::new());
    let mut zip = zip::ZipWriter::new(buf);
    let opts = zip::write::SimpleFileOptions::default().unix_permissions(0o755);
    zip.start_file(format!("{TOP}/bin/java"), opts).unwrap();
    zip.write_all(FAKE_JAVA).unwrap();
    zip.start_file(format!("{TOP}/lib/modules"), opts).unwrap();
    zip.write_all(b"modules").unwrap();
    if let Some(name) = extra {
        zip.start_file(name, opts).unwrap();
        zip.write_all(b"evil!").unwrap();
    }
    zip.finish().unwrap().into_inner()
}

/// (file name, bytes) the client accepts for the host platform: it picks the
/// extractor from the served link's extension.
fn jdk_archive(extra: Option<&str>) -> (&'static str, Vec<u8>) {
    if cfg!(windows) {
        ("temurin-jdk.zip", jdk_zip(extra))
    } else {
        ("temurin-jdk.tar.gz", jdk_tar_gz(extra))
    }
}

struct Mock {
    pkg_name: String,
    body: Vec<u8>,
    assets_json: String,
    assets_hits: AtomicU64,
    pkg_hits: AtomicU64,
}

/// Serve the assets JSON (with the given checksum) + the package bytes.
async fn spawn_mock(pkg_name: &str, pkg: Vec<u8>, checksum: Option<String>) -> (String, Arc<Mock>) {
    let assets_json = json!([{
        "release_name": TOP,
        "version": { "major": 21 },
        "binaries": [{
            "image_type": "jdk",
            "package": {
                "checksum": checksum,
                "link": pkg_name,
                "size": pkg.len(),
            }
        }]
    }])
    .to_string();
    let mock = Arc::new(Mock {
        pkg_name: pkg_name.to_string(),
        body: pkg,
        assets_json,
        assets_hits: AtomicU64::new(0),
        pkg_hits: AtomicU64::new(0),
    });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let base_for_handler = base.clone();
    let app = Router::new()
        .route(
            "/v3/assets/latest/:major/hotspot",
            routing::get(
                |UrlPath(_major): UrlPath<String>, State(m): State<Arc<Mock>>| async move {
                    m.assets_hits.fetch_add(1, Ordering::SeqCst);
                    let body = m.assets_json.replace(
                        "temurin-jdk",
                        &format!("{}/pkg/temurin-jdk", base_for_handler),
                    );
                    (
                        [(axum::http::header::CONTENT_TYPE, "application/json")],
                        body,
                    )
                },
            ),
        )
        .route(
            "/pkg/*name",
            routing::get(
                |UrlPath(name): UrlPath<String>, State(m): State<Arc<Mock>>| async move {
                    if name.trim_start_matches('/') != m.pkg_name {
                        return StatusCode::NOT_FOUND.into_response();
                    }
                    m.pkg_hits.fetch_add(1, Ordering::SeqCst);
                    m.body.clone().into_response()
                },
            ),
        )
        .with_state(mock.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, mock)
}

fn manager_for(dir: &Path, base: &str) -> JavaRuntimeManager {
    JavaRuntimeManager::with_adoptium(
        dir,
        PlatformInfo::current(),
        AdoptiumClient::with_base_url(format!("{base}/v3")),
    )
}

#[tokio::test]
async fn provisions_jdk_and_skips_second_download() {
    let (name, archive) = jdk_archive(None);
    let (base, mock) = spawn_mock(name, archive.clone(), Some(sha256_hex(&archive))).await;
    let dir = tempfile::tempdir().unwrap();
    let mgr = manager_for(dir.path(), &base);

    // progress must be observed end-to-end
    let phases: Arc<Mutex<Vec<JavaInstallPhase>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = phases.clone();
    let progress: JavaProgress = Arc::new(move |p: JavaDownloadProgress| {
        sink.lock().unwrap().push(p.phase);
    });

    let exe = mgr.ensure_java(21, Some(&progress)).await.unwrap();
    assert!(exe.is_file(), "java executable missing: {}", exe.display());
    assert_eq!(std::fs::read(&exe).unwrap(), FAKE_JAVA);
    assert_eq!(
        exe.parent().unwrap().file_name().unwrap(),
        "bin",
        "unexpected layout: {}",
        exe.display()
    );
    assert!(exe.starts_with(dir.path().join("java-21")));

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_ne!(
            std::fs::metadata(&exe).unwrap().permissions().mode() & 0o111,
            0,
            "bin/java lost its exec bit"
        );
    }

    // single top-level dir stripped; other files present
    assert!(dir.path().join("java-21/lib/modules").is_file());
    assert!(!dir.path().join("java-21/jdk-21.0.5+11").exists());

    // no temp leftovers in the runtimes dir
    for entry in std::fs::read_dir(dir.path()).unwrap() {
        let n = entry.unwrap().file_name();
        let n = n.to_string_lossy();
        assert!(!n.starts_with('.'), "leftover temp file {n}");
    }

    let seen = phases.lock().unwrap().clone();
    assert_eq!(seen.first(), Some(&JavaInstallPhase::Checking));
    assert_eq!(seen.last(), Some(&JavaInstallPhase::Done));
    assert!(seen.contains(&JavaInstallPhase::Downloading));
    assert!(seen.contains(&JavaInstallPhase::Extracting));

    // second call resolves from disk: no more HTTP at all
    let exe2 = mgr.ensure_java(21, None).await.unwrap();
    assert_eq!(exe, exe2);
    assert_eq!(mock.pkg_hits.load(Ordering::SeqCst), 1);
    assert_eq!(mock.assets_hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn checksum_mismatch_installs_nothing() {
    let (name, archive) = jdk_archive(None);
    let bogus = "0".repeat(64);
    let (base, mock) = spawn_mock(name, archive, Some(bogus)).await;
    let dir = tempfile::tempdir().unwrap();
    let mgr = manager_for(dir.path(), &base);

    let err = mgr.ensure_java(21, None).await.unwrap_err();
    assert!(err.to_string().contains("sha256 mismatch"), "{err}");
    assert!(!dir.path().join("java-21").exists());
    // the temp package existed; it must be cleaned up
    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with('.'))
        .collect();
    assert!(leftovers.is_empty(), "temp files left behind");
    assert_eq!(mock.pkg_hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn archive_path_traversal_is_rejected() {
    let (name, archive) = jdk_archive(Some("../evil"));
    let sha = sha256_hex(&archive);
    let (base, _mock) = spawn_mock(name, archive, Some(sha)).await;
    let dir = tempfile::tempdir().unwrap();
    let runtimes = dir.path().join("runtimes");
    let mgr = manager_for(&runtimes, &base);

    let err_or_ok = mgr.ensure_java(21, None).await;
    // The tar path rejects the unsafe entry; the zip reader's enclosed_name
    // filters it out earlier, so a clean install with the entry dropped is
    // equally acceptable. What must never happen is the file escaping.
    if let Err(e) = err_or_ok {
        assert!(e.to_string().contains("unsafe path"), "{e}");
    }
    assert!(!dir.path().join("evil").exists());
    assert!(!runtimes.join("evil").exists());
    assert!(!runtimes.join("java-21").join("evil").exists());
}

#[tokio::test]
#[ignore = "hits the real api.adoptium.net; run with --ignored"]
async fn live_adoptium_provision() {
    // Real assets API + GitHub download: proves the JSON shape and link
    // handling against production data.
    let dir = tempfile::tempdir().unwrap();
    let mgr = JavaRuntimeManager::new(dir.path());
    let exe = mgr.ensure_java(21, None).await.unwrap();
    assert!(exe.is_file());
    let out = std::process::Command::new(&exe)
        .arg("-version")
        .output()
        .unwrap();
    let banner = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(banner.contains("21"), "{banner}");
    println!("{banner}");
}

#[tokio::test]
async fn assets_404_is_reported() {
    let (name, archive) = jdk_archive(None);
    let sha = sha256_hex(&archive);
    let (base, _mock) = spawn_mock(name, archive, Some(sha)).await;
    let dir = tempfile::tempdir().unwrap();
    // point the client at a base without the assets route
    let mgr = JavaRuntimeManager::with_adoptium(
        dir.path(),
        PlatformInfo::current(),
        AdoptiumClient::with_base_url(format!("{base}/v2")),
    );
    let err = mgr.ensure_java(17, None).await.unwrap_err();
    assert!(err.to_string().contains("HTTP 404"), "{err}");
    assert!(!dir.path().join("java-17").exists());
}
