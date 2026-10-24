use std::path::Path;

/// Matches paths against standard user-preservation patterns.
/// Preserved files must NEVER be deleted during synchronization cleanup.
#[derive(Debug, Clone)]
pub struct WhitelistMatcher {
    patterns: Vec<String>,
}

impl Default for WhitelistMatcher {
    fn default() -> Self {
        Self::new(&[
            "saves/**",
            "saves/*",
            "screenshots/**",
            "screenshots/*",
            "resourcepacks/**",
            "shaderpacks/**",
            "logs/**",
            "crash-reports/**",
            "options.txt",
            "optionsof.txt",
            "servers.dat",
            "servers.dat_old",
            "hotbar.nbt",
            "usercache.json",
            "usernamecache.json",
        ])
    }
}

impl WhitelistMatcher {
    pub fn new(patterns: &[&str]) -> Self {
        Self {
            patterns: patterns.iter().map(|p| p.to_string()).collect(),
        }
    }

    /// Add custom glob pattern
    pub fn add_pattern(&mut self, pattern: impl Into<String>) {
        self.patterns.push(pattern.into());
    }

    /// Check if a relative path is whitelisted (preserved)
    pub fn is_whitelisted<P: AsRef<Path>>(&self, path: P) -> bool {
        let normalized = path.as_ref().to_string_lossy().replace('\\', "/");

        for pattern in &self.patterns {
            if Self::glob_matches(pattern, &normalized) {
                return true;
            }
        }
        false
    }

    fn glob_matches(pattern: &str, path: &str) -> bool {
        if pattern.ends_with("/**") {
            let prefix = &pattern[..pattern.len() - 3];
            if path == prefix || path.starts_with(&format!("{prefix}/")) {
                return true;
            }
        } else if pattern.ends_with("/*") {
            let prefix = &pattern[..pattern.len() - 2];
            if path.starts_with(&format!("{prefix}/")) {
                let remainder = &path[prefix.len() + 1..];
                return !remainder.contains('/');
            }
        } else if pattern == path {
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_whitelist() {
        let wl = WhitelistMatcher::default();
        assert!(wl.is_whitelisted("saves/world1/level.dat"));
        assert!(wl.is_whitelisted("screenshots/photo.png"));
        assert!(wl.is_whitelisted("options.txt"));
        assert!(!wl.is_whitelisted("mods/fabric-api.jar"));
        assert!(!wl.is_whitelisted("config/neat.json"));
    }
}
