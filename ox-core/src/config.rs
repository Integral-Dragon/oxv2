use std::path::{Path, PathBuf};

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
                if let Some(home) = std::env::var("HOME").ok() {
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
                if path.extension().and_then(|e| e.to_str()) == Some("toml") {
                    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                        let name = stem.to_string();
                        if seen.insert(name.clone()) {
                            results.push((name, path));
                        }
                    }
                }
            }
        }
    }

    results
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
