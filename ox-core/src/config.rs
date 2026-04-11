use serde::Deserialize;
use std::path::{Path, PathBuf};

use crate::workflow::{TriggerDef, TriggersFile};

/// Resolve the configuration search path.
/// 1. {repo}/.ox/
/// 2. Each directory in $OX_HOME (colon-separated, left to right)
pub fn resolve_search_path(repo_root: &Path) -> Vec<PathBuf> {
    let mut path = vec![];
    let repo_ox = repo_root.join(".ox");
    if repo_ox.is_dir() {
        path.push(repo_ox);
    }
    if let Ok(ox_home) = std::env::var("OX_HOME") {
        for dir in ox_home.split(':') {
            let expanded = if dir.starts_with('~') {
                if let Ok(home) = std::env::var("HOME") {
                    dir.replacen('~', &home, 1)
                } else {
                    dir.to_string()
                }
            } else {
                dir.to_string()
            };
            let p = PathBuf::from(&expanded);
            if p.is_dir() {
                path.push(p);
            }
        }
    }
    path
}

/// Find a named config file. First match wins.
pub fn find_config(search_path: &[PathBuf], subdir: &str, name: &str) -> Option<PathBuf> {
    for dir in search_path {
        let candidate = dir.join(subdir).join(format!("{name}.toml"));
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Load all config files from a subdirectory across the search path.
/// First match per name wins (higher-priority directories shadow lower ones).
pub fn load_all_configs(search_path: &[PathBuf], subdir: &str) -> Vec<(String, PathBuf)> {
    let mut seen = std::collections::HashSet::new();
    let mut results = vec![];

    for dir in search_path {
        let sub = dir.join(subdir);
        if !sub.is_dir() {
            continue;
        }
        if let Ok(entries) = std::fs::read_dir(&sub) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("toml")
                    && let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                        let name = stem.to_string();
                        if seen.insert(name.clone()) {
                            results.push((name, path));
                        }
                    }
            }
        }
    }

    results
}

// ── OxConfig ────────────────────────────────────────────────────────

const DEFAULT_HEARTBEAT_GRACE: u64 = 60;

/// Top-level ox configuration, loaded from `config.toml` in the search path.
#[derive(Debug, Clone, Deserialize)]
pub struct OxConfig {
    /// Paths to trigger files (relative to the config file's parent directory).
    #[serde(default = "default_triggers")]
    pub triggers: Vec<String>,
    /// Heartbeat grace period in seconds.
    #[serde(default = "default_heartbeat_grace")]
    pub heartbeat_grace: u64,
}

fn default_triggers() -> Vec<String> {
    vec!["workflows/triggers.toml".to_string()]
}

fn default_heartbeat_grace() -> u64 {
    DEFAULT_HEARTBEAT_GRACE
}

impl Default for OxConfig {
    fn default() -> Self {
        Self {
            triggers: default_triggers(),
            heartbeat_grace: DEFAULT_HEARTBEAT_GRACE,
        }
    }
}

/// Load and merge `config.toml` from every directory in the search path.
/// Trigger file lists are concatenated (additive). Scalar values use first-wins.
/// If no config.toml is found, returns defaults (which resolve trigger paths
/// relative to each search-path directory).
pub fn load_config(search_path: &[PathBuf]) -> OxConfig {
    let mut found_any = false;
    let mut first_heartbeat: Option<u64> = None;
    let mut trigger_paths: Vec<String> = Vec::new();

    for dir in search_path {
        let path = dir.join("config.toml");
        if !path.is_file() {
            continue;
        }
        found_any = true;

        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(path = %path.display(), err = %e, "failed to read config.toml");
                continue;
            }
        };
        let cfg: OxConfig = match toml::from_str(&content) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(path = %path.display(), err = %e, "failed to parse config.toml");
                continue;
            }
        };

        // Resolve trigger paths relative to this config file's parent dir
        for rel in &cfg.triggers {
            let abs = dir.join(rel);
            if let Some(s) = abs.to_str() {
                trigger_paths.push(s.to_string());
            }
        }

        // Scalars: first wins
        if first_heartbeat.is_none() {
            first_heartbeat = Some(cfg.heartbeat_grace);
        }
    }

    if !found_any {
        // No config.toml anywhere — resolve defaults against each search-path dir
        for dir in search_path {
            for rel in &default_triggers() {
                let abs = dir.join(rel);
                if abs.is_file()
                    && let Some(s) = abs.to_str() {
                        trigger_paths.push(s.to_string());
                    }
            }
        }
    }

    OxConfig {
        triggers: trigger_paths,
        heartbeat_grace: first_heartbeat.unwrap_or(DEFAULT_HEARTBEAT_GRACE),
    }
}

/// Load all trigger definitions from the files listed in the config.
pub fn load_triggers(config: &OxConfig) -> Vec<TriggerDef> {
    let mut triggers = Vec::new();

    for path_str in &config.triggers {
        let path = Path::new(path_str);
        if !path.is_file() {
            tracing::warn!(path = %path.display(), "trigger file not found, skipping");
            continue;
        }
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(path = %path.display(), err = %e, "failed to read trigger file");
                continue;
            }
        };
        let file: TriggersFile = match toml::from_str(&content) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(path = %path.display(), err = %e, "failed to parse trigger file");
                continue;
            }
        };
        tracing::info!(path = %path.display(), count = file.trigger.len(), "loaded triggers");
        triggers.extend(file.trigger);
    }

    triggers
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn find_config_first_match_wins() {
        let tmp = std::env::temp_dir().join("ox-config-test");
        let dir_a = tmp.join("a/runtimes");
        let dir_b = tmp.join("b/runtimes");
        fs::create_dir_all(&dir_a).unwrap();
        fs::create_dir_all(&dir_b).unwrap();

        fs::write(dir_a.join("claude.toml"), "# from a").unwrap();
        fs::write(dir_b.join("claude.toml"), "# from b").unwrap();

        let search = vec![tmp.join("a"), tmp.join("b")];
        let found = find_config(&search, "runtimes", "claude").unwrap();
        assert_eq!(found, dir_a.join("claude.toml"));

        fs::remove_dir_all(&tmp).ok();
    }
}
