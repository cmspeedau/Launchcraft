//! Java detection helpers.

/// Detect a Java executable on the current system.
pub fn detect_java() -> Option<String> {
    if let Ok(java_home) = std::env::var("JAVA_HOME") {
        let candidate = [
            std::path::Path::new(&java_home).join("bin").join("java"),
            std::path::Path::new(&java_home).join("bin").join("java.exe"),
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
    let out = std::process::Command::new(java_path).arg("-version").output().ok()?;
    let text = String::from_utf8_lossy(&out.stderr);
    text.lines().next().map(String::from)
}
