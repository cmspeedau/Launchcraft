//! Java detection helpers.

use std::path::{Path, PathBuf};

/// One Java runtime found on this machine, with its reported version.
#[derive(Debug, Clone, PartialEq)]
pub struct JavaInstall {
    pub path: String,
    /// Short version string, e.g. `21.0.5` or `1.8.0_402`.
    pub version: String,
    /// Directory name of the installation, e.g. `java-21-openjdk`.
    pub label: String,
}

impl JavaInstall {
    /// Display label for the version dropdown.
    pub fn display(&self) -> String {
        format!("Java {}  ({})", self.version, self.label)
    }
}

/// Default file/URL opener program for this OS (`xdg-open`, `open`, `explorer`).
pub fn open_program() -> &'static str {
    if cfg!(target_os = "windows") {
        "explorer"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    }
}

/// Detect a Java executable on the current system.
pub fn detect_java() -> Option<String> {
    if let Ok(java_home) = std::env::var("JAVA_HOME") {
        let candidate = [
            std::path::Path::new(&java_home).join("bin").join("java"),
            std::path::Path::new(&java_home)
                .join("bin")
                .join("java.exe"),
        ]
        .into_iter()
        .find(|p| p.exists());
        if let Some(p) = candidate {
            return Some(p.to_string_lossy().into_owned());
        }
    }
    // Fall back to `java` on PATH
    let ok = std::process::Command::new("java")
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    ok.then(|| "java".to_string())
}

/// Query the detected Java version string, if available.
pub fn version(java_path: &str) -> Option<String> {
    let out = spawn_version(java_path)?;
    let text = String::from_utf8_lossy(&out.stderr);
    text.lines().next().map(String::from)
}

/// `java -version` is side-effect free, so a transient spawn failure
/// (ETXTBSY/EAGAIN under heavy parallel load) is worth one retry.
fn spawn_version(java_path: &str) -> Option<std::process::Output> {
    for attempt in 0..2 {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
        if let Ok(out) = std::process::Command::new(java_path)
            .arg("-version")
            .output()
        {
            return Some(out);
        }
    }
    None
}

/// Extract the quoted version from a `java -version` banner line.
/// `openjdk version "21.0.5" 2024-10-15` → `Some("21.0.5")`.
pub fn parse_version(banner: &str) -> Option<String> {
    let start = banner.find('"')?;
    let rest = &banner[start + 1..];
    let end = rest.find('"')?;
    let v = &rest[..end];
    (!v.is_empty()).then(|| v.to_string())
}

/// Version of a Java binary, parsed short form ("21.0.5"), `None` if not runnable.
pub fn version_short(java_path: &str) -> Option<String> {
    version(java_path).and_then(|b| parse_version(&b))
}

/// Major version a Java executable reports (`21.0.5` → `21`, legacy `1.8.0_402`
/// → `8`). `None` when the path is not runnable or the banner is unparsable.
pub fn major_of(path: &Path) -> Option<u32> {
    let banner = version(&path.to_string_lossy())?;
    let v = parse_version(&banner)?;
    let major = version_key(&v).0;
    (major > 0).then_some(major)
}

/// Managed runtimes the launcher provisioned itself: `dir/java-<major>/bin/java`
/// (macOS `Contents/Home` layout included). Only runnable ones are listed.
pub fn managed_installs(dir: &Path) -> Vec<JavaInstall> {
    let mut installs = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return installs;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("java-") || !path.is_dir() {
            continue;
        }
        let exe = if cfg!(windows) { "java.exe" } else { "java" };
        let bin = ["bin", "Contents/Home/bin"]
            .iter()
            .map(|l| path.join(l).join(exe))
            .find(|p| p.is_file());
        let Some(bin) = bin else { continue };
        let Some(version) = version_short(&bin.to_string_lossy()) else {
            continue;
        };
        installs.push(JavaInstall {
            path: bin.to_string_lossy().into_owned(),
            version,
            label: format!("{name} (managed)"),
        });
    }
    installs.sort_by_key(|a| std::cmp::Reverse(version_key(&a.version)));
    installs
}

/// System-detected runtimes merged with the launcher's managed ones,
/// newest first; managed duplicates of the same binary are dropped.
pub fn detect_all_with_managed(runtimes_dir: &Path) -> Vec<JavaInstall> {
    let mut installs = detect_all();
    for m in managed_installs(runtimes_dir) {
        let mpath = Path::new(&m.path).canonicalize().ok();
        if !installs
            .iter()
            .any(|i| Path::new(&i.path).canonicalize().ok() == mpath && mpath.is_some())
        {
            installs.push(m);
        }
    }
    installs.sort_by_key(|a| std::cmp::Reverse(version_key(&a.version)));
    installs
}

/// Scan the standard install locations and return every runnable Java we find,
/// newest version first. `PATH` java, `JAVA_HOME`, `/usr/lib/jvm/*`, SDKMAN,
/// macOS and Windows trees are all consulted; missing ones are skipped.
pub fn detect_all() -> Vec<JavaInstall> {
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Ok(home) = std::env::var("JAVA_HOME") {
        roots.push(PathBuf::from(home));
    }
    if cfg!(target_os = "linux") {
        for dir in [
            "/usr/lib/jvm",
            "/usr/java",
            "/opt/java",
            "/usr/local/lib/jvm",
        ] {
            if let Ok(entries) = std::fs::read_dir(dir) {
                roots.extend(entries.flatten().map(|e| e.path()));
            }
        }
        if let Ok(home) = std::env::var("HOME") {
            let sdkman = Path::new(&home).join(".sdkman/candidates/java");
            if let Ok(entries) = std::fs::read_dir(sdkman) {
                roots.extend(
                    entries
                        .flatten()
                        .map(|e| e.path())
                        .filter(|p| p.file_name().map(|n| n != "current").unwrap_or(true)),
                );
            }
        }
    }
    if cfg!(target_os = "macos") {
        for dir in [
            "/Library/Java/JavaVirtualMachines",
            "/System/Library/Java/JavaVirtualMachines",
        ] {
            if let Ok(entries) = std::fs::read_dir(dir) {
                roots.extend(entries.flatten().map(|e| e.path().join("Contents/Home")));
            }
        }
    }
    if cfg!(windows) {
        for env in ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"] {
            if let Ok(pf) = std::env::var(env) {
                for vendor in ["Java", "Eclipse Adoptium", "Microsoft", "Zulu"] {
                    let dir = Path::new(&pf).join(vendor);
                    if let Ok(entries) = std::fs::read_dir(&dir) {
                        roots.extend(entries.flatten().map(|e| e.path()));
                    }
                }
            }
        }
    }

    let mut installs = scan_roots(&roots);

    // PATH java as a last entry if nothing better matched it.
    if let Some(p) = detect_java() {
        if p == "java" && version_short("java").is_some() {
            let v = version_short("java").unwrap();
            if !installs.iter().any(|i| i.version == v && i.path == p) {
                installs.push(JavaInstall {
                    path: p,
                    version: v,
                    label: "PATH".into(),
                });
            }
        }
    }

    installs.sort_by_key(|a| std::cmp::Reverse(version_key(&a.version)));
    installs
}

/// Turn a list of candidate Java homes (each expected to contain `bin/java`)
/// into installs, keeping only runnable binaries and de-duplicating symlinks.
/// Exposed for testing with a temp tree.
pub fn scan_roots(roots: &[PathBuf]) -> Vec<JavaInstall> {
    let exe = if cfg!(windows) { "java.exe" } else { "java" };
    let mut installs: Vec<JavaInstall> = Vec::new();
    let mut seen: Vec<PathBuf> = Vec::new();
    for root in roots {
        let bin = root.join("bin").join(exe);
        if !bin.is_file() {
            continue;
        }
        let canonical = bin.canonicalize().unwrap_or_else(|_| bin.clone());
        if seen.contains(&canonical) {
            continue;
        }
        seen.push(canonical);
        let path = bin.to_string_lossy().into_owned();
        let Some(v) = version_short(&path) else {
            continue;
        };
        let label = root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "java".into());
        installs.push(JavaInstall {
            path,
            version: v,
            label,
        });
    }
    installs
}

/// Major→numeric key for sorting versions newest-first; unparseable sorts last.
fn version_key(v: &str) -> (u32, u32) {
    let major = v
        .split('.')
        .next()
        .and_then(|m| m.parse::<u32>().ok())
        .unwrap_or(0);
    // Legacy "1.8.0_402" encodes the real major in the second component.
    if major == 1 {
        let real = v
            .split('.')
            .nth(1)
            .and_then(|m| m.parse::<u32>().ok())
            .unwrap_or(0);
        (real, 1)
    } else {
        (major, 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_banners() {
        assert_eq!(
            parse_version(r#"openjdk version "21.0.5" 2024-10-15"#).as_deref(),
            Some("21.0.5")
        );
        assert_eq!(
            parse_version(r#"java version "1.8.0_402" Java(TM) SE Runtime Environment"#).as_deref(),
            Some("1.8.0_402")
        );
        assert_eq!(parse_version("no quotes here"), None);
    }

    #[test]
    fn sorts_newest_first_with_legacy_versions() {
        assert!(version_key("21.0.1") > version_key("17.0.10"));
        assert!(version_key("11.0.2") > version_key("1.8.0_402"));
        assert!(version_key("1.8.0_402") > version_key("1.7.0_80"));
    }

    #[test]
    fn scan_roots_finds_only_runnable_installs() {
        let tmp = tempfile::tempdir().unwrap();
        let good = tmp.path().join("fake-java-99");
        std::fs::create_dir_all(good.join("bin")).unwrap();
        std::fs::write(
            good.join("bin/java"),
            "#!/bin/sh\necho 'openjdk version \"99.1.0\" 2099-01-01' >&2\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                good.join("bin/java"),
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap();
        }
        // Broken candidate: exists but not runnable → skipped.
        let bad = tmp.path().join("broken");
        std::fs::create_dir_all(bad.join("bin")).unwrap();
        std::fs::write(bad.join("bin/java"), "not an executable").unwrap();
        // Empty candidate: no bin at all → skipped.
        let empty = tmp.path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();

        let installs = scan_roots(&[good.clone(), bad, empty]);
        assert_eq!(installs.len(), 1);
        assert_eq!(installs[0].version, "99.1.0");
        assert_eq!(installs[0].label, "fake-java-99");
        assert_eq!(installs[0].path, good.join("bin/java").to_string_lossy());
    }

    #[cfg(unix)]
    #[test]
    fn major_of_reads_stub_banners() {
        let tmp = tempfile::tempdir().unwrap();
        let mk = |name: &str, banner: &str| {
            let p = tmp.path().join(name);
            let content = format!("#!/bin/sh\nprintf '%s\\n' '{banner}' >&2\n");
            std::fs::write(&p, content).unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
            p
        };
        assert_eq!(
            major_of(&mk("j21", "openjdk version \"21.0.5\" 2024-10-15")),
            Some(21)
        );
        assert_eq!(major_of(&mk("j8", "java version \"1.8.0_402\"")), Some(8));
        assert_eq!(major_of(&mk("junk", "not a banner")), None);
        assert_eq!(major_of(&tmp.path().join("missing")), None);
    }

    #[cfg(unix)]
    #[test]
    fn managed_installs_lists_runnable_runtimes() {
        let tmp = tempfile::tempdir().unwrap();
        let rt21 = tmp.path().join("java-21");
        std::fs::create_dir_all(rt21.join("bin")).unwrap();
        std::fs::write(
            rt21.join("bin/java"),
            "#!/bin/sh\necho 'openjdk version \"21.0.5\" 2024-10-15' >&2\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            rt21.join("bin/java"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        // Not a managed layout: skipped.
        std::fs::create_dir_all(tmp.path().join("random-dir")).unwrap();
        // Managed dir without a runnable bin: skipped.
        std::fs::create_dir_all(tmp.path().join("java-17/bin")).unwrap();

        let installs = managed_installs(tmp.path());
        assert_eq!(installs.len(), 1);
        assert_eq!(installs[0].version, "21.0.5");
        assert_eq!(installs[0].label, "java-21 (managed)");

        // Missing directory yields an empty list, not an error.
        assert!(managed_installs(&tmp.path().join("nope")).is_empty());
    }

    #[test]
    fn detect_all_is_sorted_and_parsable() {
        let installs = detect_all();
        assert!(installs
            .windows(2)
            .all(|w| version_key(&w[0].version) >= version_key(&w[1].version)));
        for i in &installs {
            assert!(!i.version.is_empty());
            // Absolute scan hits must exist; the PATH fallback is the bare "java".
            if i.path != "java" {
                assert!(Path::new(&i.path).exists(), "{}", i.path);
            }
        }
    }
}
