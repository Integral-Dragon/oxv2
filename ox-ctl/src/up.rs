//! Local ensemble orchestration: server + herder + seguro runners.
//!
//! This is the Rust port of the old `bin/ox-up` bash script. The pure
//! pieces (path derivation, pidfile parsing, binary resolution, seguro
//! argv) live here with unit tests. The spawning/killing/fs side effects
//! are tested by running `ox-ctl up` against a real build.

use anyhow::{Context, Result, anyhow, bail};
use ox_core::client::OxClient;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

// ── Path layout ──────────────────────────────────────────────────────

/// All paths ox writes under a project's `.ox/run/` directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunPaths {
    pub run_dir: PathBuf,
    pub log_dir: PathBuf,
    pub scripts_dir: PathBuf,
    pub pidfile: PathBuf,
    pub db: PathBuf,
}

impl RunPaths {
    /// Build the standard layout for `repo/.ox/run/`.
    pub fn for_repo(repo: &Path) -> Self {
        let run_dir = repo.join(".ox").join("run");
        Self {
            log_dir: run_dir.join("logs"),
            scripts_dir: run_dir.join("scripts"),
            pidfile: run_dir.join("ox.pids"),
            db: run_dir.join("ox.db"),
            run_dir,
        }
    }

    /// Path of the runner workspace directory for runner `n` (1-indexed).
    pub fn runner_workspace(&self, n: usize) -> PathBuf {
        self.run_dir.join(format!("runner-{n}"))
    }
}

// ── Pidfile ──────────────────────────────────────────────────────────

/// A single process entry in the pidfile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PidEntry {
    pub pid: u32,
    pub name: String,
}

impl PidEntry {
    pub fn format_line(&self) -> String {
        format!("{} {}\n", self.pid, self.name)
    }
}

/// Parse the pidfile format: one entry per line, `<pid> <name>`. Blank
/// lines and malformed entries are skipped silently.
pub fn parse_pidfile(content: &str) -> Vec<PidEntry> {
    content
        .lines()
        .filter_map(|line| {
            let mut parts = line.trim().splitn(2, ' ');
            let pid = parts.next()?.parse::<u32>().ok()?;
            let name = parts.next()?.trim().to_string();
            if name.is_empty() {
                None
            } else {
                Some(PidEntry { pid, name })
            }
        })
        .collect()
}

/// True if a process with `pid` is alive (best effort: `kill(pid, 0)`).
#[cfg(unix)]
pub fn is_running(pid: u32) -> bool {
    // SAFETY: kill with signal 0 has no side effect besides error reporting.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

#[cfg(not(unix))]
pub fn is_running(_pid: u32) -> bool {
    false
}

// ── Binary resolution ────────────────────────────────────────────────

/// Sibling binaries that `ox-ctl up` needs to launch. `ox_runner` and
/// `ox_rt` aren't spawned directly — they come along for the ride in the
/// `bin_dir` read-only mount seguro shares into each runner VM — but we
/// still verify their presence up front so the user gets a clean error
/// instead of a cryptic runner-VM failure later.
#[derive(Debug, Clone)]
pub struct Binaries {
    pub ox_server: PathBuf,
    pub ox_herder: PathBuf,
    pub bin_dir: PathBuf,
}

/// Resolve the sibling binaries relative to a given `bin_dir`. Returns an
/// error if any of the three expected binaries is missing. Callers pass
/// `current_exe().parent()` in production; tests pass a scratch dir.
pub fn resolve_binaries_in(bin_dir: &Path) -> Result<Binaries> {
    let ox_server = bin_dir.join("ox-server");
    let ox_herder = bin_dir.join("ox-herder");
    let ox_runner = bin_dir.join("ox-runner");
    let ox_rt = bin_dir.join("ox-rt");
    for (name, path) in [
        ("ox-server", &ox_server),
        ("ox-herder", &ox_herder),
        ("ox-runner", &ox_runner),
        ("ox-rt", &ox_rt),
    ] {
        if !path.is_file() {
            bail!(
                "{name} not found at {}; run `cargo build` or install ox via `cargo install --git …`",
                path.display(),
            );
        }
    }
    Ok(Binaries {
        ox_server,
        ox_herder,
        bin_dir: bin_dir.to_path_buf(),
    })
}

// ── Seguro runner argv ───────────────────────────────────────────────

/// Arguments to pass to `seguro run` for one ox-runner VM.
///
/// The runner is invoked inside a guest bash so we can set HOME and PATH
/// before exec'ing `/ox/bin/ox-runner`. Matches the layout in the old
/// bin/ox-up script: ox binaries at /ox/bin, cx at /ox/scripts.
/// Per-user shared sccache cache directory: `$HOME/.cache/ox/sccache`.
/// Created if missing. Shared across every project the user runs ox in so
/// dependency builds (tokio, hyper, etc.) cache-hit across repositories.
pub fn sccache_cache_dir() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").ok_or_else(|| anyhow!("HOME not set"))?;
    let dir = PathBuf::from(home).join(".cache/ox/sccache");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("create sccache cache dir {}", dir.display()))?;
    Ok(dir)
}

pub fn seguro_runner_argv(
    bin_dir: &Path,
    scripts_dir: &Path,
    sccache_dir: &Path,
    server_url: &str,
) -> Result<Vec<String>> {
    let bin_s = bin_dir
        .to_str()
        .ok_or_else(|| anyhow!("bin_dir is not utf-8"))?;
    let scripts_s = scripts_dir
        .to_str()
        .ok_or_else(|| anyhow!("scripts_dir is not utf-8"))?;
    let _ = sccache_dir;
    // Single-quoted so $HOME / $PATH expand inside the guest, not out here.
    let guest_cmd = format!(
        "export HOME=/home/agent && \
         export PATH=/ox/bin:/ox/scripts:$HOME/.cargo/bin:$PATH && \
         /ox/bin/ox-runner --server {server_url} --environment seguro --workspace-dir /tmp/ox-work"
    );
    Ok(vec![
        "run".into(),
        "--share".into(),
        format!("{bin_s}:/ox/bin:ro"),
        "--share".into(),
        format!("{scripts_s}:/ox/scripts:ro"),
        "--net".into(),
        "dev-bridge".into(),
        "--unsafe-dev-bridge".into(),
        "--persistent".into(),
        "--".into(),
        "bash".into(),
        "-c".into(),
        guest_cmd,
    ])
}

// ── Cx staging ───────────────────────────────────────────────────────

/// Locate the `cx` binary on a PATH-style env value. Returns `None` if
/// cx isn't in any of the listed directories. Tests pass a synthetic
/// PATH so they don't have to mutate the process environment.
pub fn find_cx_in_path(path: &std::ffi::OsStr) -> Option<PathBuf> {
    for dir in std::env::split_paths(path) {
        let candidate = dir.join("cx");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Copy `cx` into `scripts_dir/cx` so the seguro mount (`/ox/scripts:ro`)
/// carries it into every runner VM. Returns the destination path. Noop
/// with warning if cx isn't on PATH.
pub fn stage_cx_binary(scripts_dir: &Path) -> Result<Option<PathBuf>> {
    let Some(path) = std::env::var_os("PATH") else {
        return Ok(None);
    };
    stage_cx_binary_from(scripts_dir, &path)
}

/// Test hook for [`stage_cx_binary`] — takes an explicit PATH to avoid
/// racing on the process-wide environment.
pub fn stage_cx_binary_from(
    scripts_dir: &Path,
    path: &std::ffi::OsStr,
) -> Result<Option<PathBuf>> {
    let Some(src) = find_cx_in_path(path) else {
        return Ok(None);
    };
    std::fs::create_dir_all(scripts_dir)
        .with_context(|| format!("create {}", scripts_dir.display()))?;
    let dst = scripts_dir.join("cx");
    std::fs::copy(&src, &dst).with_context(|| format!("copy {} -> {}", src.display(), dst.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(Some(dst))
}

// ── Commands ─────────────────────────────────────────────────────────

/// Start the local ensemble: server, herder, seguro runners. Seeds
/// claude_credentials if the host has them. Writes pidfile and logs to
/// `.ox/run/`. Returns after the children are spawned.
pub async fn cmd_up(runners: usize, port: u16) -> Result<()> {
    let repo = std::env::current_dir().context("cwd")?;
    let paths = RunPaths::for_repo(&repo);

    if paths.pidfile.is_file() {
        let content = std::fs::read_to_string(&paths.pidfile).unwrap_or_default();
        let alive = parse_pidfile(&content)
            .into_iter()
            .filter(|e| is_running(e.pid))
            .count();
        if alive > 0 {
            bail!(
                "ox is already running ({alive} processes). Run `ox-ctl down` first.",
            );
        }
        // Stale pidfile — clear it.
        let _ = std::fs::remove_file(&paths.pidfile);
    }

    let current_exe = std::env::current_exe().context("current_exe")?;
    let bin_dir = current_exe
        .parent()
        .ok_or_else(|| anyhow!("current_exe has no parent dir"))?;
    let bins = resolve_binaries_in(bin_dir)?;

    // Check seguro is on PATH — spawn will fail with a less helpful error
    // otherwise.
    if which("seguro").is_none() {
        bail!("seguro not found on PATH — see https://github.com/dragon-panic/seguro");
    }

    std::fs::create_dir_all(&paths.run_dir)?;
    std::fs::create_dir_all(&paths.log_dir)?;
    std::fs::create_dir_all(&paths.scripts_dir)?;
    // Truncate pidfile.
    File::create(&paths.pidfile)?;

    let project_name = repo
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");
    println!(
        "starting ox for {project_name} (repo={}, port={port})",
        repo.display()
    );
    println!();

    // ── ox-server ────────────────────────────────────────────────
    let server_log = paths.log_dir.join("server.log");
    let server_pid = spawn_detached(
        &bins.ox_server,
        &[
            "--port".into(),
            port.to_string(),
            "--repo".into(),
            repo.to_string_lossy().to_string(),
            "--db".into(),
            paths.db.to_string_lossy().to_string(),
        ],
        &server_log,
    )?;
    append_pid(&paths.pidfile, &PidEntry { pid: server_pid, name: "server".into() })?;
    println!("  server    pid={server_pid}  port={port}");

    // Give the server a moment to bind its port before the herder and
    // secret-seeding hit it.
    std::thread::sleep(std::time::Duration::from_millis(500));

    // ── Seed claude_credentials ──────────────────────────────────
    let server_url = format!("http://localhost:{port}");
    match seed_claude_credentials(&server_url).await {
        Ok(true) => println!("  secrets   claude_credentials seeded"),
        Ok(false) => println!("  warning   ~/.claude/.credentials.json not found — claude steps will fail"),
        Err(e) => println!("  warning   failed to seed claude_credentials: {e}"),
    }

    // ── Seed codex_auth ──────────────────────────────────────────
    match seed_codex_credentials(&server_url).await {
        Ok(true) => println!("  secrets   codex_auth seeded"),
        Ok(false) => println!("  note      ~/.codex/auth.json not found — codex steps will fail (run `codex login`)"),
        Err(e) => println!("  warning   failed to seed codex_auth: {e}"),
    }

    // ── ox-herder ────────────────────────────────────────────────
    let herder_log = paths.log_dir.join("herder.log");
    let herder_pid = spawn_detached(
        &bins.ox_herder,
        &["--server".into(), server_url.clone()],
        &herder_log,
    )?;
    append_pid(
        &paths.pidfile,
        &PidEntry { pid: herder_pid, name: "herder".into() },
    )?;
    println!("  herder    pid={herder_pid}");

    // ── Stage cx into scripts dir ────────────────────────────────
    match stage_cx_binary(&paths.scripts_dir) {
        Ok(Some(_)) => {}
        Ok(None) => println!("  warning   cx not found on PATH — workflows can't update cx state"),
        Err(e) => println!("  warning   failed to stage cx: {e}"),
    }

    // ── Runners (seguro VMs) ─────────────────────────────────────
    // Runners reach the host via QEMU's user-mode gateway at 10.0.2.2.
    let guest_server = format!("http://10.0.2.2:{port}");
    for i in 1..=runners {
        let workspace = paths.runner_workspace(i);
        std::fs::create_dir_all(&workspace)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&workspace, std::fs::Permissions::from_mode(0o777));
        }
        let log = paths.log_dir.join(format!("runner-{i}.log"));
        let args = seguro_runner_argv(
            &bins.bin_dir,
            &paths.scripts_dir,
            &sccache_cache_dir()?,
            &guest_server,
        )?;
        let pid = spawn_detached(Path::new("seguro"), &args, &log)?;
        append_pid(
            &paths.pidfile,
            &PidEntry { pid, name: format!("runner-{i}") },
        )?;
        println!("  runner-{i}  pid={pid}  (seguro) workspace={}", workspace.display());
    }

    println!();
    println!("logs in {}", paths.log_dir.display());
    println!("pidfile {}", paths.pidfile.display());
    println!();
    println!("next steps:");
    println!("  ox-ctl status");
    println!("  ox-ctl events");

    Ok(())
}

/// Stop the local ensemble. Reads the pidfile, sends SIGTERM to every
/// alive entry, wipes runner workspaces, and prunes seguro sessions.
pub fn cmd_down() -> Result<()> {
    let repo = std::env::current_dir().context("cwd")?;
    let paths = RunPaths::for_repo(&repo);

    if !paths.pidfile.is_file() {
        println!("ox is not running (no pidfile)");
        return Ok(());
    }

    println!("stopping ox...");
    let content = std::fs::read_to_string(&paths.pidfile)?;
    for entry in parse_pidfile(&content) {
        if is_running(entry.pid) {
            #[cfg(unix)]
            unsafe {
                libc::kill(entry.pid as libc::pid_t, libc::SIGTERM);
            }
            println!("  killed {} (pid={})", entry.name, entry.pid);
        }
    }

    let _ = std::fs::remove_file(&paths.pidfile);
    // Wipe runner workspaces.
    if let Ok(entries) = std::fs::read_dir(&paths.run_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            if name.to_string_lossy().starts_with("runner-") {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }
    // Best-effort seguro sessions prune.
    let _ = Command::new("seguro")
        .args(["sessions", "prune"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    println!("done");
    Ok(())
}

/// Wipe the database and logs. Refuses to run while the ensemble is up.
pub fn cmd_reset() -> Result<()> {
    let repo = std::env::current_dir().context("cwd")?;
    let paths = RunPaths::for_repo(&repo);

    if paths.pidfile.is_file() {
        let content = std::fs::read_to_string(&paths.pidfile).unwrap_or_default();
        let alive = parse_pidfile(&content)
            .into_iter()
            .filter(|e| is_running(e.pid))
            .count();
        if alive > 0 {
            bail!("ox is still running ({alive} processes) — run `ox-ctl down` first");
        }
    }

    println!("resetting ox state...");
    for name in ["ox.db", "ox.db-wal", "ox.db-shm"] {
        let _ = std::fs::remove_file(paths.run_dir.join(name));
    }
    let _ = std::fs::remove_dir_all(&paths.log_dir);
    println!("done — next `ox-ctl up` will begin fresh");
    Ok(())
}

// ── Spawning helpers ─────────────────────────────────────────────────

/// Spawn a child process fully detached: no tty, stdin from /dev/null,
/// stdout+stderr redirected to `log_path`, in its own session via
/// `setsid()`. The child outlives this ox-ctl invocation.
fn spawn_detached(program: &Path, args: &[String], log_path: &Path) -> Result<u32> {
    use std::os::unix::process::CommandExt;

    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .with_context(|| format!("open log {}", log_path.display()))?;
    let log_err = log.try_clone()?;

    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(log_err);
    // SAFETY: setsid has no invariants we need to uphold beyond "call in
    // the child between fork and exec" — which pre_exec guarantees.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                // Already a session leader — not fatal, but log to stderr
                // of the child (which is redirected to the log file).
                let _ = writeln!(
                    std::io::stderr(),
                    "ox-ctl: setsid failed: {}",
                    std::io::Error::last_os_error()
                );
            }
            Ok(())
        });
    }
    let child = cmd
        .spawn()
        .with_context(|| format!("spawn {}", program.display()))?;
    Ok(child.id())
}

fn append_pid(pidfile: &Path, entry: &PidEntry) -> Result<()> {
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(pidfile)?;
    f.write_all(entry.format_line().as_bytes())?;
    Ok(())
}

/// Locate a binary by name on $PATH.
fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

async fn seed_claude_credentials(server_url: &str) -> Result<bool> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let Some(home) = home else {
        return Ok(false);
    };
    let cred_path = home.join(".claude").join(".credentials.json");
    if !cred_path.is_file() {
        return Ok(false);
    }
    let value = std::fs::read_to_string(&cred_path)
        .with_context(|| format!("read {}", cred_path.display()))?;
    let client = OxClient::new(server_url);
    client
        .set_secret("claude_credentials", &value)
        .await
        .context("set_secret claude_credentials")?;
    Ok(true)
}

async fn seed_codex_credentials(server_url: &str) -> Result<bool> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let Some(home) = home else {
        return Ok(false);
    };
    let auth_path = home.join(".codex").join("auth.json");
    if !auth_path.is_file() {
        return Ok(false);
    }
    let value = std::fs::read_to_string(&auth_path)
        .with_context(|| format!("read {}", auth_path.display()))?;
    let client = OxClient::new(server_url);
    client
        .set_secret("codex_auth", &value)
        .await
        .context("set_secret codex_auth")?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(label: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "ox-ctl-up-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    // ── RunPaths ────────────────────────────────────────────────────

    #[test]
    fn run_paths_lays_out_run_dir_correctly() {
        let repo = PathBuf::from("/tmp/fake-repo");
        let paths = RunPaths::for_repo(&repo);
        assert_eq!(paths.run_dir, PathBuf::from("/tmp/fake-repo/.ox/run"));
        assert_eq!(paths.log_dir, PathBuf::from("/tmp/fake-repo/.ox/run/logs"));
        assert_eq!(paths.pidfile, PathBuf::from("/tmp/fake-repo/.ox/run/ox.pids"));
        assert_eq!(paths.db, PathBuf::from("/tmp/fake-repo/.ox/run/ox.db"));
        assert_eq!(
            paths.scripts_dir,
            PathBuf::from("/tmp/fake-repo/.ox/run/scripts")
        );
        assert_eq!(
            paths.runner_workspace(2),
            PathBuf::from("/tmp/fake-repo/.ox/run/runner-2")
        );
    }

    // ── Pidfile ─────────────────────────────────────────────────────

    #[test]
    fn pidfile_roundtrip() {
        let entries = vec![
            PidEntry { pid: 123, name: "server".into() },
            PidEntry { pid: 456, name: "herder".into() },
            PidEntry { pid: 789, name: "runner-1".into() },
        ];
        let content: String = entries.iter().map(|e| e.format_line()).collect();
        let parsed = parse_pidfile(&content);
        assert_eq!(parsed, entries);
    }

    #[test]
    fn pidfile_skips_malformed_lines() {
        let content = "\
            123 server\n\
            \n\
            garbage\n\
            456\n\
            789 runner-1\n\
        ";
        let parsed = parse_pidfile(content);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].name, "server");
        assert_eq!(parsed[1].name, "runner-1");
    }

    #[test]
    fn is_running_reports_own_pid_alive() {
        assert!(is_running(std::process::id()));
    }

    #[test]
    fn is_running_reports_bogus_pid_dead() {
        // pid 0 is invalid; kill(0, 0) targets the caller's process group
        // and would falsely report alive. Use a very high pid that shouldn't
        // exist.
        assert!(!is_running(u32::MAX - 1));
    }

    // ── Binary resolution ───────────────────────────────────────────

    #[test]
    fn resolve_binaries_finds_siblings() {
        let dir = tmp("bins-ok");
        for name in ["ox-server", "ox-herder", "ox-runner", "ox-rt"] {
            std::fs::write(dir.join(name), b"#!/bin/sh\n").unwrap();
        }
        let bins = resolve_binaries_in(&dir).unwrap();
        assert_eq!(bins.ox_server, dir.join("ox-server"));
        assert_eq!(bins.ox_herder, dir.join("ox-herder"));
        assert_eq!(bins.bin_dir, dir);
    }

    #[test]
    fn resolve_binaries_errors_when_any_missing() {
        let dir = tmp("bins-missing");
        std::fs::write(dir.join("ox-server"), b"").unwrap();
        // no herder, runner, or rt
        let err = resolve_binaries_in(&dir).unwrap_err();
        assert!(err.to_string().contains("ox-herder"), "got: {err}");
    }

    #[test]
    fn resolve_binaries_errors_when_ox_rt_missing() {
        let dir = tmp("bins-no-rt");
        for name in ["ox-server", "ox-herder", "ox-runner"] {
            std::fs::write(dir.join(name), b"").unwrap();
        }
        // ox-rt deliberately missing
        let err = resolve_binaries_in(&dir).unwrap_err();
        assert!(err.to_string().contains("ox-rt"), "got: {err}");
    }

    // ── Seguro argv ─────────────────────────────────────────────────

    #[test]
    fn seguro_runner_argv_builds_expected_shape() {
        let bin = PathBuf::from("/ox-bin");
        let scripts = PathBuf::from("/ox-scripts");
        let sccache = PathBuf::from("/sccache-host");
        let args =
            seguro_runner_argv(&bin, &scripts, &sccache, "http://10.0.2.2:4840").unwrap();
        assert_eq!(args[0], "run");
        // Shares in the correct order and format.
        let joined = args.join(" ");
        assert!(joined.contains("--share /ox-bin:/ox/bin:ro"));
        assert!(joined.contains("--share /ox-scripts:/ox/scripts:ro"));
        // sccache share is writable (no :ro suffix). Guest path /cache/sccache
        // matches v1's convention so SCCACHE_DIR stays the same.
        assert!(
            joined.contains("--share /sccache-host:/cache/sccache"),
            "missing sccache share in: {joined}"
        );
        assert!(
            !joined.contains("/sccache-host:/cache/sccache:ro"),
            "sccache share must not be read-only"
        );
        // Dev bridge + unsafe flags present.
        assert!(joined.contains("--net dev-bridge"));
        assert!(joined.contains("--unsafe-dev-bridge"));
        assert!(joined.contains("--persistent"));
        // End-of-options sentinel, then bash -c <guest-cmd>.
        let sep = args.iter().position(|a| a == "--").expect("-- sentinel");
        assert_eq!(args[sep + 1], "bash");
        assert_eq!(args[sep + 2], "-c");
        let guest = &args[sep + 3];
        assert!(guest.contains("/ox/bin/ox-runner"));
        assert!(guest.contains("--server http://10.0.2.2:4840"));
        assert!(guest.contains("--environment seguro"));
        assert!(guest.contains("HOME=/home/agent"));
        // sccache env vars for rustc wrapping inside the guest.
        assert!(
            guest.contains("SCCACHE_DIR=/cache/sccache"),
            "missing SCCACHE_DIR export in guest cmd: {guest}"
        );
        assert!(
            guest.contains("RUSTC_WRAPPER=sccache"),
            "missing RUSTC_WRAPPER export in guest cmd: {guest}"
        );
    }

    // ── Cx staging ──────────────────────────────────────────────────

    #[test]
    fn stage_cx_binary_copies_to_scripts_dir() {
        let fake_bin = tmp("cx-src");
        let fake_cx = fake_bin.join("cx");
        std::fs::write(&fake_cx, b"#!/bin/sh\necho cx\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake_cx, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let scripts = tmp("cx-dst");
        let staged = stage_cx_binary_from(&scripts, fake_bin.as_os_str()).unwrap();
        assert_eq!(staged, Some(scripts.join("cx")));
        assert!(scripts.join("cx").is_file());
    }

    #[test]
    fn stage_cx_binary_returns_none_when_cx_missing() {
        let empty = tmp("cx-empty");
        let scripts = tmp("cx-dst2");
        let staged = stage_cx_binary_from(&scripts, empty.as_os_str()).unwrap();
        assert_eq!(staged, None);
    }
}
