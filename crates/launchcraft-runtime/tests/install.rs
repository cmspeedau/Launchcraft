//! Install + launch-planning pipeline against a local mock of Mojang
//! piston-meta, the fabric meta API, and the blob hosts: verifies the
//! on-disk layout, sha1 enforcement, cache skips, loader overlay merges
//! (all three library dialects) and the substituted launch arguments.

use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::{routing, Router};
use serde_json::json;
use sha1::{Digest, Sha1};

use launchcraft_core::paths::LauncherPaths;
use launchcraft_runtime::meta::{
    build_game_args, LaunchIdentity, LaunchSpec, LoaderKind, MinecraftClient,
};
use launchcraft_runtime::PlatformInfo;

fn sha1_hex(bytes: &[u8]) -> String {
    let mut h = Sha1::new();
    h.update(bytes);
    let out = h.finalize();
    out.iter().map(|b| format!("{b:02x}")).collect()
}

/// Natives jar: one .so plus a META-INF entry that must be skipped, and
/// a zip-slip entry name that must abort extraction.
fn natives_jar(extra: Option<&str>) -> Vec<u8> {
    let buf = std::io::Cursor::new(Vec::new());
    let mut zip = zip::ZipWriter::new(buf);
    let opts = zip::write::SimpleFileOptions::default();
    zip.start_file("libminecraft.so", opts).unwrap();
    zip.write_all(b"\x7fELF-fake-native").unwrap();
    zip.start_file("META-INF/MANIFEST.MF", opts).unwrap();
    zip.write_all(b"Manifest-Version: 1.0\n").unwrap();
    if let Some(name) = extra {
        zip.start_file(name, opts).unwrap();
        zip.write_all(b"escape").unwrap();
    }
    zip.finish().unwrap().into_inner()
}

struct States {
    list: String,
    profiles: HashMap<String, String>,
    blobs: HashMap<String, Vec<u8>>,
}

async fn spawn_mock() -> String {
    let client_jar = b"client-jar-bytes".to_vec();
    let lwjgl_jar = b"lwjgl-core-bytes".to_vec();
    let logging_jar = b"logging-bytes".to_vec();
    let loader_jar = b"fabric-loader-bytes".to_vec();
    let log4j_jar = b"log4j-bytes".to_vec();
    let apache_jar = b"commons-bytes".to_vec();
    let natives = natives_jar(None);
    let asset_a: Vec<u8> = b"asset A".to_vec();
    let asset_b: Vec<u8> = b"asset B longer".to_vec();
    let hash_a = sha1_hex(&asset_a);
    let hash_b = sha1_hex(&asset_b);

    let index_json = json!({
        "objects": {
            "minecraft/lang/en_us.json": { "hash": hash_a, "size": asset_a.len() },
            "minecraft/sounds/ambient.wav": { "hash": hash_b, "size": asset_b.len() },
        }
    })
    .to_string();
    let index_sha1 = sha1_hex(index_json.as_bytes());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());

    let mut blobs: HashMap<String, Vec<u8>> = HashMap::new();
    blobs.insert("client-1.20.4.jar".into(), client_jar.clone());
    blobs.insert("lwjgl-3.3.2.jar".into(), lwjgl_jar.clone());
    blobs.insert("lwjgl-3.3.2-natives-linux.jar".into(), natives.clone());
    blobs.insert("logging-1.1.1.jar".into(), logging_jar.clone());
    blobs.insert("fabric-loader-0.15.11.jar".into(), loader_jar.clone());
    blobs.insert("log4j-api-2.19.0.jar".into(), log4j_jar.clone());
    blobs.insert("commons-lang3-3.12.0.jar".into(), apache_jar.clone());
    blobs.insert("index-17.json".into(), index_json.into_bytes());
    blobs.insert(hash_a.clone(), asset_a);
    blobs.insert(hash_b.clone(), asset_b);

    let asset_index = json!({
        "id": "17", "sha1": index_sha1, "size": 999, "totalSize": 21,
        "url": format!("{base}/piston/index-17.json"),
    });

    let vanilla = json!({
        "id": "1.20.4",
        "type": "release",
        "mainClass": "net.minecraft.client.main.Main",
        "downloads": { "client": {
            "sha1": sha1_hex(&client_jar),
            "size": client_jar.len(),
            "url": format!("{base}/piston/client-1.20.4.jar"),
        }},
        "assetIndex": asset_index,
        "assets": "17",
        "javaVersion": { "component": "java-runtime-gamma", "majorVersion": 21 },
        "libraries": [
            { "name": "org.lwjgl:lwjgl:3.3.2",
              "downloads": {
                "artifact": {
                    "path": "org/lwjgl/lwjgl/3.3.2/lwjgl-3.3.2.jar",
                    "sha1": sha1_hex(&lwjgl_jar), "size": lwjgl_jar.len(),
                    "url": format!("{base}/piston/lwjgl-3.3.2.jar"),
                },
                "classifiers": { "natives-linux": {
                    "path": "org/lwjgl/lwjgl/3.3.2/lwjgl-3.3.2-natives-linux.jar",
                    "sha1": sha1_hex(&natives), "size": natives.len(),
                    "url": format!("{base}/piston/lwjgl-3.3.2-natives-linux.jar"),
                }}
              },
              "natives": { "linux": "natives-linux" }
            },
            { "name": "com.mojang:logging:1.1.1",
              "downloads": { "artifact": {
                  "path": "com/mojang/logging/1.1.1/logging-1.1.1.jar",
                  "sha1": sha1_hex(&logging_jar), "size": logging_jar.len(),
                  "url": format!("{base}/piston/logging-1.1.1.jar"),
              }}},
            { "name": "org.lwjgl:lwjgl-fake:1.0",
              "downloads": { "artifact": {
                  "path": "org/lwjgl/lwjgl-fake/1.0/lwjgl-fake-1.0.jar",
                  "url": format!("{base}/piston/never-served.jar"),
              }},
              "rules": [{ "action": "allow" }, {
                  "action": "disallow", "os": { "name": "linux" } }]
            },
            { "name": "com.mojang:old-legacy:1.0",
              "extract": { "exclude": ["META-INF/"] },
              "url": format!("{base}/piston/legacy.jar")
            }
        ],
        "arguments": { "game": [
            "--username", "${auth_player_name}",
            "--version", "${version_name}",
            "--gameDir", "${game_directory}",
            "--assetsDir", "${assets_root}",
            "--assetIndex", "${assets_index_name}",
            "--uuid", "${auth_uuid}",
            "--accessToken", "${auth_access_token}",
            { "rules": [{ "action": "allow", "features": { "is_demo_enabled": {} } }],
              "value": "--demo" },
            { "rules": [{ "action": "allow" }], "value": ["--guid", "${user_properties}"] }
        ]},
    });

    // Same vanilla layer but the client jar's sha1 is a lie: must abort.
    let mut bad = vanilla.clone();
    bad["downloads"]["client"]["sha1"] = json!("0000000000000000000000000000000000000000");
    bad["id"] = json!("bad-sha");

    // Fabric overlay exercising every loader library dialect:
    //  1. full launchermeta `downloads.artifact`,
    //  2. fabric-meta `url` repo + explicit `maven` path,
    //  3. bare `group:artifact:version` coordinates + repo `url`.
    let fabric = json!({
        "id": "fabric-loader-0.15.11-1.20.4",
        "inheritsFrom": "1.20.4",
        "mainClass": "net.fabricmc.loader.impl.launch.knot.KnotClient",
        "libraries": [
            { "name": "net.fabricmc:fabric-loader:0.15.11",
              "downloads": { "artifact": {
                  "path": "net/fabricmc/fabric-loader/0.15.11/fabric-loader-0.15.11.jar",
                  "sha1": sha1_hex(&loader_jar), "size": loader_jar.len(),
                  "url": format!("{base}/piston/fabric-loader-0.15.11.jar"),
              }}},
            { "name": "org.apache.logging.log4j:log4j-api:2.19.0",
              "url": format!("{base}/maven2/"),
              "maven": "org/apache/logging/log4j/log4j-api/2.19.0/log4j-api-2.19.0.jar" },
            { "name": "org.apache:commons-lang3:3.12.0",
              "url": format!("{base}/maven2/") }
        ],
        "arguments": { "game": [
            { "rules": [{ "action": "allow" }], "value": "--fabric-only" }
        ]},
    });

    let list = json!({
        "latest": { "release": "1.20.4", "snapshot": "1.20.4" },
        "versions": [
            { "id": "1.20.4", "type": "release",
              "url": format!("{base}/profiles/1.20.4.json"),
              "time": "2023-12-07T12:00:00+00:00",
              "releaseTime": "2023-12-07T12:00:00+00:00" },
            { "id": "bad-sha", "type": "release",
              "url": format!("{base}/profiles/bad-sha.json"),
              "time": "2023-12-07T12:00:00+00:00",
              "releaseTime": "2023-12-07T12:00:00+00:00" }
        ]
    })
    .to_string();

    let mut profiles = HashMap::new();
    profiles.insert("1.20.4".to_string(), vanilla.to_string());
    profiles.insert("bad-sha".to_string(), bad.to_string());
    profiles.insert("fabric/0.15.11-1.20.4".to_string(), fabric.to_string());

    let state = Arc::new(States {
        list,
        profiles,
        blobs,
    });
    fn json_tuple(body: String) -> ([(axum::http::HeaderName, &'static str); 1], String) {
        (
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            body,
        )
    }
    let app =
        Router::new()
            .route(
                "/mc/game/version_manifest_v2.json",
                routing::get(
                    |State(s): State<Arc<States>>| async move { json_tuple(s.list.clone()) },
                ),
            )
            .route(
                "/profiles/:id",
                routing::get(
                    |UrlPath(id): UrlPath<String>, State(s): State<Arc<States>>| async move {
                        let key = id.trim_end_matches(".json");
                        match s.profiles.get(key) {
                            Some(p) => json_tuple(p.clone()).into_response(),
                            None => StatusCode::NOT_FOUND.into_response(),
                        }
                    },
                ),
            )
            .route(
                "/versions/loader/:mc/:loader/profile/json",
                routing::get(
                    |UrlPath((mc, loader)): UrlPath<(String, String)>,
                     State(s): State<Arc<States>>| async move {
                        match s.profiles.get(&format!("fabric/{loader}-{mc}")) {
                            Some(p) => json_tuple(p.clone()).into_response(),
                            None => StatusCode::NOT_FOUND.into_response(),
                        }
                    },
                ),
            )
            .route(
                "/versions/loader/minecraft/:mc",
                routing::get(|UrlPath(mc): UrlPath<String>| async move {
                    if mc == "1.20.4" {
                        json_tuple(
                            json!([{ "loader": { "version": "0.15.11", "stable": true } }])
                                .to_string(),
                        )
                        .into_response()
                    } else {
                        StatusCode::NOT_FOUND.into_response()
                    }
                }),
            )
            .route(
                "/piston/:name",
                routing::get(
                    |UrlPath(name): UrlPath<String>, State(s): State<Arc<States>>| async move {
                        match s.blobs.get(&name) {
                            Some(b) => b.clone().into_response(),
                            None => StatusCode::NOT_FOUND.into_response(),
                        }
                    },
                ),
            )
            .route(
                "/maven2/*rest",
                routing::get(
                    |UrlPath(rest): UrlPath<String>, State(s): State<Arc<States>>| async move {
                        let tail = rest.rsplit('/').next().unwrap_or(&rest);
                        match s.blobs.get(tail) {
                            Some(b) => b.clone().into_response(),
                            None => StatusCode::NOT_FOUND.into_response(),
                        }
                    },
                ),
            )
            .route(
                "/resources/:two/:hash",
                routing::get(
                    |UrlPath((_two, hash)): UrlPath<(String, String)>,
                     State(s): State<Arc<States>>| async move {
                        match s.blobs.get(&hash) {
                            Some(b) => b.clone().into_response(),
                            None => StatusCode::NOT_FOUND.into_response(),
                        }
                    },
                ),
            );
    tokio::spawn(async move { axum::serve(listener, app.with_state(state)).await.unwrap() });
    base
}

fn test_paths(root: &Path) -> LauncherPaths {
    LauncherPaths {
        root: root.to_path_buf(),
        instances: root.join("instances"),
        cas_cache: root.join("cas"),
        runtimes: root.join("runtimes"),
        versions: root.join("versions"),
        assets: root.join("assets"),
        libraries: root.join("libraries"),
        config: root.join("config"),
        is_portable: true,
    }
}

fn client_for(base: &str, root: &Path) -> MinecraftClient {
    MinecraftClient::with_urls(
        test_paths(root),
        base.to_string(),
        base.to_string(),
        base.to_string(),
        format!("{base}/resources"),
    )
}

#[tokio::test]
async fn install_then_plan_launch_with_fabric_overlay() {
    let base = spawn_mock().await;
    let tmp = tempfile::tempdir().unwrap();
    let mc = client_for(&base, tmp.path());

    let list = mc.version_list().await.unwrap();
    let entry = list.find("1.20.4").unwrap().clone();
    assert_eq!(
        mc.loader_versions(LoaderKind::Fabric, "1.20.4")
            .await
            .unwrap(),
        vec!["0.15.11".to_string()]
    );

    // ---- vanilla install ------------------------------------------------
    let report = mc.install_version(&entry, &list, true, None).await.unwrap();
    assert_eq!(report.version, "1.20.4");
    // client jar + 2 assets + 3 library files (lwjgl, natives, logging)
    assert_eq!(report.files_written, 6, "got {report:?}");
    let vdir = tmp.path().join("versions/1.20.4");
    assert!(vdir.join("1.20.4.json").is_file());
    assert_eq!(
        std::fs::read(vdir.join("1.20.4.jar")).unwrap(),
        b"client-jar-bytes"
    );
    assert!(tmp.path().join("assets/indexes/17.json").is_file());
    let hash_a = sha1_hex(b"asset A");
    assert!(tmp
        .path()
        .join(format!("assets/objects/{}/{hash_a}", &hash_a[..2]))
        .is_file());
    assert!(tmp
        .path()
        .join("libraries/org/lwjgl/lwjgl/3.3.2/lwjgl-3.3.2.jar")
        .is_file());
    assert!(tmp
        .path()
        .join("libraries/org/lwjgl/lwjgl/3.3.2/lwjgl-3.3.2-natives-linux.jar")
        .is_file());
    assert!(tmp
        .path()
        .join("libraries/com/mojang/logging/1.1.1/logging-1.1.1.jar")
        .is_file());
    // Rule-disallowed and extract-only libraries were never fetched.
    assert!(!tmp
        .path()
        .join("libraries/org/lwjgl/lwjgl-fake/1.0/lwjgl-fake-1.0.jar")
        .exists());

    // ---- re-install skips everything --------------------------------------
    let again = mc.install_version(&entry, &list, true, None).await.unwrap();
    assert_eq!(again.files_written, 0);
    assert_eq!(again.bytes_downloaded, 0);
    assert_eq!(again.files_skipped, 6);

    // ---- fabric overlay install ------------------------------------------
    let rep = mc
        .install_loader(LoaderKind::Fabric, "1.20.4", "0.15.11", &list, false, None)
        .await
        .unwrap();
    let overlay_id = "1.20.4-fabric-0.15.11";
    assert_eq!(rep.version, overlay_id);
    assert!(mc.is_installed(overlay_id));
    assert!(mc.installed_ids().contains(&overlay_id.to_string()));
    assert_eq!(
        std::fs::read(
            tmp.path()
                .join(format!("versions/{overlay_id}/{overlay_id}.jar"))
        )
        .unwrap(),
        b"client-jar-bytes",
        "overlay gets its own copy of the client jar"
    );
    // All three library dialects landed in maven layout.
    assert!(tmp
        .path()
        .join("libraries/net/fabricmc/fabric-loader/0.15.11/fabric-loader-0.15.11.jar")
        .is_file());
    assert!(tmp
        .path()
        .join("libraries/org/apache/logging/log4j/log4j-api/2.19.0/log4j-api-2.19.0.jar")
        .is_file());
    assert!(tmp
        .path()
        .join("libraries/org/apache/commons-lang3/3.12.0/commons-lang3-3.12.0.jar")
        .is_file());

    // Classpath: overlay libraries first, then base libraries (skipping
    // the linux-disallowed and extract-only entries), client jar last.
    let merged = mc.resolved_profile(overlay_id).unwrap();
    assert_eq!(
        merged.main_class,
        "net.fabricmc.loader.impl.launch.knot.KnotClient"
    );
    let platform = PlatformInfo::current();
    let cp = merged.resolve_classpath(
        &tmp.path().join("libraries"),
        &tmp.path()
            .join(format!("versions/{overlay_id}/{overlay_id}.jar")),
        &platform,
    );
    let names: Vec<String> = cp
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        names,
        vec![
            "fabric-loader-0.15.11.jar".to_string(),
            "log4j-api-2.19.0.jar".to_string(),
            "commons-lang3-3.12.0.jar".to_string(),
            "lwjgl-3.3.2.jar".to_string(),
            "logging-1.1.1.jar".to_string(),
            format!("{overlay_id}.jar"),
        ]
    );

    // ---- launch planning (also covers merged game args) ------------------
    let instance = tmp.path().join("instances/test");
    let spec = LaunchSpec {
        version_id: overlay_id,
        instance_dir: &instance,
        java_binary: PathBuf::from("/usr/bin/java"),
        min_memory_mb: 1024,
        max_memory_mb: 4096,
        jvm_extra: vec!["-XX:+UseG1GC".into()],
        identity: LaunchIdentity {
            username: "Steve".into(),
            uuid: "0b451e80-2c7f-4f1e-9b9e-3f1a2b3c4d5e".into(),
            token: "mc-token".into(),
            xuid: String::new(),
            client_id: "client-1".into(),
        },
    };
    let plan = mc.plan_launch(&spec, &platform).unwrap();
    assert_eq!(plan.java_major, Some(21));
    assert_eq!(plan.java_component.as_deref(), Some("java-runtime-gamma"));
    // Natives unpacked zip-slip safe, META-INF skipped.
    assert_eq!(plan.natives_extracted, 1);
    assert!(instance.join("natives/libminecraft.so").is_file());
    assert!(!instance.join("natives/META-INF/MANIFEST.MF").exists());
    assert!(instance.join("minecraft").is_dir());

    let args = plan.config.build_args(&platform);
    let joined = args.join(" ");
    assert!(joined.contains("-Xms1024m -Xmx4096m"));
    assert!(joined.contains("-XX:+UseG1GC"));
    assert!(joined.contains("net.fabricmc.loader.impl.launch.knot.KnotClient"));
    // Overlay game args precede the vanilla block, placeholders resolved.
    let game_pos = args.iter().position(|a| a == "--fabric-only").unwrap();
    assert_eq!(args[game_pos + 1], "--username");
    assert_eq!(args[game_pos + 2], "Steve");
    assert!(joined.contains(&format!("--version {overlay_id}")));
    assert!(joined.contains("--accessToken mc-token"));
    assert!(joined.contains(&instance.join("minecraft").display().to_string()));
    // Feature-gated --demo never appears; ${user_properties} renders {}.
    let game: Vec<&str> = args[game_pos..].iter().map(String::as_str).collect();
    assert!(!game.contains(&"--demo"));
    let guid_pos = game.iter().position(|a| *a == "--guid").unwrap();
    assert_eq!(game[guid_pos + 1], "{}");
    assert!(!joined.contains("${"), "unresolved placeholder in {joined}");

    // Merged args are also directly renderable (unit-level check).
    let vars = HashMap::from([
        ("auth_player_name".to_string(), "Steve".to_string()),
        ("version_name".to_string(), overlay_id.to_string()),
        ("user_properties".to_string(), "{}".to_string()),
        ("game_directory".to_string(), "/tmp/gd".to_string()),
        ("assets_root".to_string(), "/tmp/ar".to_string()),
        ("assets_index_name".to_string(), "17".to_string()),
        ("auth_uuid".to_string(), "u".to_string()),
        ("auth_access_token".to_string(), "t".to_string()),
    ]);
    let rendered = build_game_args(&merged.game_args, &platform, &vars);
    assert_eq!(rendered.first().unwrap(), "--fabric-only");
}

#[tokio::test]
async fn sha1_mismatch_and_missing_profile_fail_install() {
    let base = spawn_mock().await;
    let tmp = tempfile::tempdir().unwrap();
    let mc = client_for(&base, tmp.path());
    let list = mc.version_list().await.unwrap();

    let bad = list.find("bad-sha").unwrap().clone();
    let err = mc
        .install_version(&bad, &list, false, None)
        .await
        .expect_err("lying client jar sha1 must abort the install");
    assert!(err.to_string().contains("sha1 mismatch"), "got {err}");
    // The (valid) profile itself was still cached before the jar fetch.
    assert!(tmp.path().join("versions/bad-sha/bad-sha.json").is_file());

    let mut ghost = bad.clone();
    ghost.url = format!("{base}/profiles/does-not-exist.json");
    let err = mc
        .install_version(&ghost, &list, false, None)
        .await
        .expect_err("404 profile must fail");
    assert!(err.to_string().contains("download failed"), "got {err}");
}

#[tokio::test]
async fn natives_extraction_refuses_zip_slip() {
    let jar_bytes = natives_jar(Some("../escape.so"));
    let tmp = tempfile::tempdir().unwrap();
    let jar_dir = tmp.path().join("libs");
    std::fs::create_dir_all(&jar_dir).unwrap();
    let jar = jar_dir.join("evil-natives.jar");
    std::fs::write(&jar, &jar_bytes).unwrap();

    let profile: launchcraft_runtime::VersionManifest = serde_json::from_value(json!({
        "id": "1.0",
        "mainClass": "Test",
        "libraries": [{
            "name": "test:evil:1.0",
            "downloads": { "classifiers": { "natives-linux": {
                "path": "evil-natives.jar", "url": "http://127.0.0.1:1/never"
            }}},
            "natives": { "linux": "natives-linux" }
        }]
    }))
    .unwrap();
    assert!(profile.libraries[0].is_applicable(&PlatformInfo::current()));

    let dest = tmp.path().join("natives");
    // On non-linux hosts the classifier never applies and nothing runs.
    let result = launchcraft_runtime::meta::extract_natives(
        &profile.libraries,
        &PlatformInfo::current(),
        &jar_dir,
        &dest,
    );
    if PlatformInfo::current().os == launchcraft_runtime::OperatingSystem::Linux {
        let err = result.expect_err("zip-slip entry must abort extraction");
        assert!(err.to_string().contains("unsafe zip entry"), "got {err}");
        assert!(!tmp.path().join("escape.so").exists());
    } else {
        assert_eq!(result.unwrap(), 0);
    }
}
