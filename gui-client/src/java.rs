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
    let out = std::process::Command::new(java_path)
        .arg("-version")
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stderr);
    text.lines().next().map(String::from)
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
