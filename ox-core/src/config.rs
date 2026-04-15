use include_dir::{Dir, include_dir};
use serde::Deserialize;
use std::path::{Path, PathBuf};

use crate::workflow::{TriggerDef, TriggersFile};

/// Defaults baked into the binary at compile time. The on-disk `defaults/`
/// directory in the source tree is the source of truth; `cargo build` snapshots
/// it into every binary so installed copies don't need the source repo on disk.
static EMBEDDED_DEFAULTS: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/../defaults");

/// Extract embedded defaults into `base/defaults/` if missing or stale.
///
/// `base` is typically `~/.ox`. On first run this writes every file from the
/// binary's embedded copy, marks them read-only (`0o444`), and stamps
/// `defaults/.version` with a fingerprint. On subsequent runs, if the
/// fingerprint differs, the directory is wiped and re-extracted. The fact
/// that the files are read-only is a convention signal — `chmod +w` still
/// works — but it's enough to stop casual edits and make upgrades safe.
pub fn ensure_defaults_extracted(base: &Path) -> std::io::Result<PathBuf> {
    let defaults_dir = base.join("defaults");
    let version_file = defaults_dir.join(".version");
    let want = embedded_fingerprint();

    let is_current = std::fs::read_to_string(&version_file)
        .map(|s| s.trim() == want)
        .unwrap_or(false);

    if is_current {
        return Ok(defaults_dir);
    }

    if defaults_dir.exists() {
        // Files are 0o444 — remove_dir_all needs write perms on the parent
        // dir entries. Unix remove_dir_all handles this, but flip perms back
        // first to be safe on odd filesystems.
        make_writable(&defaults_dir)?;
        std::fs::remove_dir_all(&defaults_dir)?;
    }
    std::fs::create_dir_all(&defaults_dir)?;

    extract_dir(EMBEDDED_DEFAULTS.path(), &EMBEDDED_DEFAULTS, &defaults_dir)?;
    // Stamp the version before locking down permissions — once the parent
    // dir is 0o555 we can't create new entries. Next-run upgrades call
    // make_writable first, so the cycle repeats cleanly.
    std::fs::write(&version_file, &want)?;
    mark_readonly(&defaults_dir)?;

    Ok(defaults_dir)
}

fn embedded_fingerprint() -> String {
    use std::hash::{DefaultHasher, Hasher};
    let mut h = DefaultHasher::new();
    hash_dir(&EMBEDDED_DEFAULTS, &mut h);
    format!("{}-{:x}", env!("CARGO_PKG_VERSION"), h.finish())
}

fn hash_dir(dir: &Dir<'_>, h: &mut impl std::hash::Hasher) {
    // include_dir walks in a deterministic order, so the hash is stable.
    for file in dir.files() {
        h.write(file.path().as_os_str().as_encoded_bytes());
        h.write(file.contents());
    }
    for sub in dir.dirs() {
        hash_dir(sub, h);
    }
}

fn extract_dir(root: &Path, dir: &Dir<'_>, target: &Path) -> std::io::Result<()> {
    for sub in dir.dirs() {
        extract_dir(root, sub, target)?;
    }
    for file in dir.files() {
        let rel = file
            .path()
            .strip_prefix(root)
            .unwrap_or_else(|_| file.path());
        let out = target.join(rel);
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&out, file.contents())?;
    }
    Ok(())
}

#[cfg(unix)]
fn mark_readonly(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    for entry in walk(dir)? {
        let meta = std::fs::metadata(&entry)?;
        let mode = if meta.is_dir() { 0o555 } else { 0o444 };
        std::fs::set_permissions(&entry, std::fs::Permissions::from_mode(mode))?;
    }
    Ok(())
}

#[cfg(unix)]
fn make_writable(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    for entry in walk(dir)? {
        let meta = std::fs::metadata(&entry)?;
        let mode = if meta.is_dir() { 0o755 } else { 0o644 };
        // Ignore errors — best effort.
        let _ = std::fs::set_permissions(&entry, std::fs::Permissions::from_mode(mode));
    }
    Ok(())
}

#[cfg(not(unix))]
fn mark_readonly(_dir: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(not(unix))]
fn make_writable(_dir: &Path) -> std::io::Result<()> {
    Ok(())
}

fn walk(root: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut out = vec![root.to_path_buf()];
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path.clone());
            }
            out.push(path);
        }
    }
    Ok(out)
}

/// Resolve the configuration search path.
/// 1. {repo}/.ox/
/// 2. Each directory in $OX_HOME (colon-separated, left to right)
/// 3. ~/.ox/defaults/ (extracted from the binary on first run)
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
    if let Ok(home) = std::env::var("HOME") {
        let defaults = PathBuf::from(home).join(".ox").join("defaults");
        if defaults.is_dir() {
            path.push(defaults);
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

/// Peek at a TOML file to see if it looks like a workflow definition.
///
/// Returns `Ok(true)` if the file parses as TOML and has a top-level
/// `[workflow]` table, `Ok(false)` if it parses as TOML but has no such
/// table (e.g. a trigger file that legitimately lives in `workflows/`),
/// and `Err(_)` if the file can't be read or isn't valid TOML at all.
///
/// Callers use this to skip non-workflow files silently during workflow
/// loading while still surfacing real breakage — an `Ok(false)` result
/// means "intentionally not a workflow, don't warn"; an `Err` means
/// "something is actually wrong, warn about it."
pub fn is_workflow_file(path: &Path) -> anyhow::Result<bool> {
    let content = std::fs::read_to_string(path)?;
    let value: toml::Value = toml::from_str(&content)?;
    Ok(value.get("workflow").is_some())
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
    /// Names of event-source watchers `ox-ctl up` should launch
    /// alongside the server. Each name resolves to `ox-<name>-watcher`
    /// in the same directory as `ox-server`. Additive across the
    /// config.toml search path and de-duplicated on name.
    #[serde(default)]
    pub watchers: Vec<String>,
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
            watchers: Vec::new(),
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

    // Watchers: additive across the search path, de-duped on first
    // occurrence so two layers declaring `["cx"]` don't spawn two
    // processes.
    let mut watchers: Vec<String> = Vec::new();
    {
        let mut seen = std::collections::HashSet::new();
        for dir in search_path {
            let path = dir.join("config.toml");
            let Ok(content) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(cfg) = toml::from_str::<OxConfig>(&content) else {
                continue;
            };
            for name in cfg.watchers {
                if seen.insert(name.clone()) {
                    watchers.push(name);
                }
            }
        }
    }

    OxConfig {
        triggers: trigger_paths,
        heartbeat_grace: first_heartbeat.unwrap_or(DEFAULT_HEARTBEAT_GRACE),
        watchers,
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

    fn tmp_base(label: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "ox-core-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    // ── watchers (slice 4 of event-sources migration) ─────────────

    #[test]
    fn load_config_parses_watchers_from_config_toml() {
        let dir = tmp_base("watchers-parse");
        fs::write(
            dir.join("config.toml"),
            r#"
            triggers = []
            watchers = ["cx"]
            "#,
        )
        .unwrap();

        let cfg = load_config(std::slice::from_ref(&dir));
        assert_eq!(cfg.watchers, vec!["cx".to_string()]);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_config_merges_watchers_additively_across_search_path() {
        let a = tmp_base("watchers-a");
        let b = tmp_base("watchers-b");
        fs::write(
            a.join("config.toml"),
            r#"
            triggers = []
            watchers = ["cx"]
            "#,
        )
        .unwrap();
        fs::write(
            b.join("config.toml"),
            r#"
            triggers = []
            watchers = ["linear", "github"]
            "#,
        )
        .unwrap();

        let cfg = load_config(&[a.clone(), b.clone()]);
        assert_eq!(
            cfg.watchers,
            vec!["cx".to_string(), "linear".to_string(), "github".to_string()]
        );
        fs::remove_dir_all(&a).ok();
        fs::remove_dir_all(&b).ok();
    }

    /// A project that declares the same watcher in both its local
    /// config.toml and the embedded defaults must only spawn ONE
    /// watcher process. De-dup is first-wins, so the repo-local
    /// ordering is preserved.
    #[test]
    fn load_config_dedups_watchers_declared_in_two_layers() {
        let a = tmp_base("watchers-dedup-a");
        let b = tmp_base("watchers-dedup-b");
        fs::write(
            a.join("config.toml"),
            r#"
            triggers = []
            watchers = ["cx"]
            "#,
        )
        .unwrap();
        fs::write(
            b.join("config.toml"),
            r#"
            triggers = []
            watchers = ["cx"]
            "#,
        )
        .unwrap();

        let cfg = load_config(&[a.clone(), b.clone()]);
        assert_eq!(cfg.watchers, vec!["cx".to_string()]);
        fs::remove_dir_all(&a).ok();
        fs::remove_dir_all(&b).ok();
    }

    #[test]
    fn load_config_watchers_defaults_to_empty() {
        let dir = tmp_base("watchers-empty");
        fs::write(dir.join("config.toml"), "triggers = []\n").unwrap();
        let cfg = load_config(std::slice::from_ref(&dir));
        assert!(cfg.watchers.is_empty());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ensure_defaults_extracted_writes_embedded_files() {
        let base = tmp_base("extract-fresh");
        let defaults = ensure_defaults_extracted(&base).expect("extract");
        // A sentinel file we know is in defaults/: workflows/triggers.toml.
        let triggers = defaults.join("workflows/triggers.toml");
        assert!(triggers.is_file(), "missing {triggers:?}");
        // runtime and persona files land too.
        assert!(defaults.join("runtimes/claude.toml").is_file());
        // .version exists.
        assert!(defaults.join(".version").is_file());
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn ensure_defaults_extracted_is_idempotent_when_fingerprint_matches() {
        let base = tmp_base("extract-idem");
        let defaults = ensure_defaults_extracted(&base).unwrap();
        let marker = defaults.join("workflows/triggers.toml");
        let mtime1 = fs::metadata(&marker).unwrap().modified().unwrap();
        // Second call must not rewrite the files.
        std::thread::sleep(std::time::Duration::from_millis(10));
        ensure_defaults_extracted(&base).unwrap();
        let mtime2 = fs::metadata(&marker).unwrap().modified().unwrap();
        assert_eq!(mtime1, mtime2, "second call should not touch files");
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn ensure_defaults_extracted_refreshes_on_stale_version() {
        let base = tmp_base("extract-stale");
        let defaults = ensure_defaults_extracted(&base).unwrap();

        // Everything under defaults/ is locked 0o444 / 0o555 — loosen so we
        // can plant a junk file and overwrite .version.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for entry in walk(&defaults).unwrap() {
                let meta = fs::metadata(&entry).unwrap();
                let mode = if meta.is_dir() { 0o755 } else { 0o644 };
                fs::set_permissions(&entry, fs::Permissions::from_mode(mode)).unwrap();
            }
        }

        let junk = defaults.join("workflows/junk.toml");
        fs::write(&junk, "junk").unwrap();
        fs::write(defaults.join(".version"), "bogus").unwrap();

        ensure_defaults_extracted(&base).unwrap();
        assert!(!junk.exists(), "junk should have been wiped");
        assert!(defaults.join("workflows/triggers.toml").is_file());
        fs::remove_dir_all(&base).ok();
    }

    #[cfg(unix)]
    #[test]
    fn ensure_defaults_extracted_marks_files_read_only() {
        use std::os::unix::fs::PermissionsExt;
        let base = tmp_base("extract-readonly");
        let defaults = ensure_defaults_extracted(&base).unwrap();
        let triggers = defaults.join("workflows/triggers.toml");
        let mode = fs::metadata(&triggers).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o444, "expected 0o444, got {mode:o}");
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn resolve_search_path_includes_extracted_defaults() {
        let base = tmp_base("searchpath");
        ensure_defaults_extracted(&base).unwrap();
        // Point HOME at our temp base so ~/.ox/defaults resolves there.
        let prev_home = std::env::var("HOME").ok();
        let prev_ox_home = std::env::var("OX_HOME").ok();
        // Need base itself to be $HOME/.ox/defaults layout: so set HOME such
        // that $HOME/.ox/defaults == base/defaults. Easiest: construct a new
        // temp HOME and extract there.
        let fake_home = tmp_base("searchpath-home");
        let ox_home_dir = fake_home.join(".ox");
        fs::create_dir_all(&ox_home_dir).unwrap();
        ensure_defaults_extracted(&ox_home_dir).unwrap();

        // SAFETY: tests run single-threaded when they touch env; use serial
        // via mutex if needed. For now, unsafe { set_var } once and restore.
        unsafe {
            std::env::set_var("HOME", &fake_home);
            std::env::remove_var("OX_HOME");
        }
        let repo = tmp_base("searchpath-repo");
        let path = resolve_search_path(&repo);
        let expected = fake_home.join(".ox/defaults");
        assert!(
            path.iter().any(|p| p == &expected),
            "search path {path:?} should include {expected:?}"
        );

        unsafe {
            match prev_home {
                Some(h) => std::env::set_var("HOME", h),
                None => std::env::remove_var("HOME"),
            }
            match prev_ox_home {
                Some(h) => std::env::set_var("OX_HOME", h),
                None => std::env::remove_var("OX_HOME"),
            }
        }
        fs::remove_dir_all(&base).ok();
        fs::remove_dir_all(&fake_home).ok();
        fs::remove_dir_all(&repo).ok();
    }

    // ── is_workflow_file ─────────────────────────────────────────
    //
    // Regression: load_all_configs returns every *.toml in workflows/,
    // so the workflow loader tries to parse triggers.toml as a workflow
    // and emits a misleading "missing field `workflow`" warning on
    // every server start. is_workflow_file lets the loader silently
    // skip any file that doesn't declare a [workflow] block — but real
    // broken workflow files (with a [workflow] block and other bugs)
    // still fall through to the full parse path and warn loudly.

    #[test]
    fn is_workflow_file_true_for_file_with_workflow_table() {
        let tmp = tmp_base("peek-yes");
        let path = tmp.join("code-task.toml");
        fs::write(
            &path,
            r#"
[workflow]
name = "code-task"

[[step]]
name = "one"
"#,
        )
        .unwrap();
        assert!(is_workflow_file(&path).unwrap());
        fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn is_workflow_file_false_for_triggers_toml() {
        // A triggers file has no [workflow] table — it's a [[trigger]]
        // array. Loader should skip silently without warning.
        let tmp = tmp_base("peek-no");
        let path = tmp.join("triggers.toml");
        fs::write(
            &path,
            r#"
[[trigger]]
on       = "cx.task_ready"
tag      = "workflow:code-task"
workflow = "code-task"
"#,
        )
        .unwrap();
        assert!(!is_workflow_file(&path).unwrap());
        fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn is_workflow_file_err_on_invalid_toml() {
        // Broken TOML is a real problem — the loader should still warn
        // loudly rather than silently skipping.
        let tmp = tmp_base("peek-broken");
        let path = tmp.join("broken.toml");
        fs::write(&path, "[this is not = valid = toml").unwrap();
        assert!(is_workflow_file(&path).is_err());
        fs::remove_dir_all(&tmp).ok();
    }

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
