use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use clap::{Parser, Subcommand};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

#[derive(Parser, Debug)]
#[command(name = "ox-rt", about = "runtime interface helper")]
struct Cli {
    #[command(subcommand)]
    cmd: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Complete the step with an output value.
    Done {
        /// Skip git preflight checks.
        #[arg(long)]
        force: bool,
        /// Output value (e.g. "pass", "fail:lint").
        output: Vec<String>,
    },
    /// Report a metric.
    Metric { name: String, value: String },
    /// Write artifact content. With no args, reads from stdin.
    Artifact {
        name: String,
        content: Vec<String>,
    },
    /// Close an artifact stream.
    ArtifactDone { name: String },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let socket = std::env::var("OX_SOCKET").context("OX_SOCKET not set")?;
    run(cli, Path::new(&socket))
}

fn run(cli: Cli, socket: &Path) -> Result<()> {
    match cli.cmd {
        Command::Metric { name, value } => send(socket, &format!("metric {name} {value}")),
        Command::Done { force, output } => {
            if !force {
                let cwd = std::env::current_dir().context("cwd")?;
                if let Err(msg) = preflight_done(&cwd) {
                    eprintln!("{msg}");
                    bail!("ox-rt done refused; re-run with --force to bypass");
                }
            }
            let msg = if output.is_empty() {
                "done".to_string()
            } else {
                format!("done {}", output.join(" "))
            };
            send(socket, &msg)
        }
        Command::Artifact { name, content } => {
            let bytes = if content.is_empty() {
                let mut buf = Vec::new();
                std::io::stdin().read_to_end(&mut buf)?;
                buf
            } else {
                content.join(" ").into_bytes()
            };
            let encoded = BASE64.encode(&bytes);
            send(socket, &format!("artifact {name} {encoded}"))
        }
        Command::ArtifactDone { name } => send(socket, &format!("artifact-done {name}")),
    }
}

/// Check whether it's safe to report `done` from the current workdir.
///
/// The next workflow step does a fresh clone of `origin/<branch>` — anything
/// uncommitted, unpushed, or absent from the remote is invisible to it.
/// Returns `Ok(())` when it's safe to proceed (including when there's no git
/// repo at all, or when we're on `main`/detached HEAD, since those aren't
/// agent work branches). Returns `Err(message)` with a multi-line diagnostic
/// suitable for printing to stderr.
fn preflight_done(workdir: &Path) -> std::result::Result<(), String> {
    let rev_parse = git(workdir, &["rev-parse", "--git-dir"]);
    if !rev_parse.is_ok_and(|o| o.status.success()) {
        return Ok(());
    }

    let branch = match git(workdir, &["rev-parse", "--abbrev-ref", "HEAD"]) {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).trim().to_string(),
        _ => return Ok(()),
    };
    if branch == "HEAD" || branch == "main" {
        return Ok(());
    }

    let status = git(workdir, &["status", "--porcelain"])
        .map_err(|e| format!("error: git status failed: {e}"))?;
    if !status.stdout.is_empty() {
        return Err(format!(
            "error: ox-rt done refused — working tree is dirty.\n\n\
             You have uncommitted changes in {cwd}. The next step clones a fresh\n\
             workspace from origin and will never see them. Commit and push:\n\n\
             \x20   git add <files>\n\
             \x20   git commit -m \"...\"\n\
             \x20   git push origin {branch}\n\n\
             Then re-run `ox-rt done <output>`. If you really mean to abandon\n\
             these changes, re-run with --force.",
            cwd = workdir.display(),
        ));
    }

    // Best effort — network may be down, origin may be slow, not fatal.
    let _ = git(workdir, &["fetch", "origin", &branch]);
    let _ = git(workdir, &["fetch", "origin", "main"]);

    let local_head = git(workdir, &["rev-parse", "HEAD"])
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .map_err(|e| format!("error: git rev-parse HEAD failed: {e}"))?;

    let remote_ref = format!("origin/{branch}");
    let remote_head = git(workdir, &["rev-parse", "--verify", &remote_ref])
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());

    let Some(remote_head) = remote_head else {
        return Err(format!(
            "error: ox-rt done refused — origin/{branch} does not exist.\n\n\
             Branch '{branch}' has not been pushed to origin. The next step\n\
             clones from origin and will never see your work. Push:\n\n\
             \x20   git push origin {branch}\n\n\
             Then re-run `ox-rt done <output>`. If you really mean to report\n\
             done without pushing, re-run with --force."
        ));
    };

    if local_head != remote_head {
        let ahead = git(workdir, &["rev-list", "--count", &format!("{remote_ref}..HEAD")])
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_else(|| "?".into());
        return Err(format!(
            "error: ox-rt done refused — local HEAD is ahead of origin/{branch} by {ahead} commit(s).\n\n\
             Your push did not happen, or did not reach the remote. The next step\n\
             clones from origin/{branch} and will never see these commits. Push:\n\n\
             \x20   git push origin {branch}\n\n\
             Then re-run `ox-rt done <output>`. If you really mean to report done\n\
             without pushing, re-run with --force."
        ));
    }

    let ahead_of_main = git(
        workdir,
        &["rev-list", "--count", &format!("origin/main..{remote_ref}")],
    )
    .ok()
    .and_then(|o| {
        String::from_utf8_lossy(&o.stdout)
            .trim()
            .parse::<u64>()
            .ok()
    })
    .unwrap_or(0);

    if ahead_of_main == 0 {
        return Err(format!(
            "error: ox-rt done refused — origin/{branch} has no commits ahead of origin/main.\n\n\
             There is nothing on this branch for the next step to review. You may\n\
             have committed and pushed to the wrong branch, or never committed at\n\
             all. Investigate, then re-run `ox-rt done <output>`.\n\n\
             If you really mean to report done with no work to show, re-run with\n\
             --force."
        ));
    }

    Ok(())
}

fn git(workdir: &Path, args: &[&str]) -> std::io::Result<std::process::Output> {
    std::process::Command::new("git")
        .current_dir(workdir)
        .args(args)
        .output()
}

fn send(socket: &Path, msg: &str) -> Result<()> {
    let mut stream = UnixStream::connect(socket)
        .with_context(|| format!("connect to {}", socket.display()))?;
    stream.write_all(msg.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;

    let mut reader = BufReader::new(&stream);
    let mut resp = String::new();
    reader.read_line(&mut resp)?;
    let resp = resp.trim();
    if let Some(err) = resp.strip_prefix("error:") {
        bail!("ox-runner rejected command:{err}");
    }
    if resp != "ok" {
        return Err(anyhow!("unexpected response from ox-runner: {resp}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use std::sync::mpsc;
    use std::thread;

    /// Start a one-shot listener on a temp socket. Returns the path and a receiver
    /// that will produce the first line written by the client.
    fn start_listener() -> (std::path::PathBuf, mpsc::Receiver<String>) {
        let dir = std::env::temp_dir().join(format!("ox-rt-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(format!(
            "sock-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).expect("bind");
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                if reader.read_line(&mut line).is_ok() {
                    let _ = tx.send(line);
                }
                let mut writer = stream;
                let _ = writer.write_all(b"ok\n");
            }
        });
        (path, rx)
    }

    /// Listener that replies with a caller-chosen response.
    fn start_listener_with_response(
        response: &'static [u8],
    ) -> (std::path::PathBuf, mpsc::Receiver<String>) {
        let dir = std::env::temp_dir().join(format!("ox-rt-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(format!(
            "sock-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).expect("bind");
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                if reader.read_line(&mut line).is_ok() {
                    let _ = tx.send(line);
                }
                let mut writer = stream;
                let _ = writer.write_all(response);
            }
        });
        (path, rx)
    }

    #[test]
    fn metric_sends_newline_terminated_message() {
        let (path, rx) = start_listener();
        let cli = Cli::parse_from(["ox-rt", "metric", "input_tokens", "14523"]);
        run(cli, &path).expect("run");
        assert_eq!(rx.recv().unwrap(), "metric input_tokens 14523\n");
    }

    #[test]
    fn done_with_force_sends_done_with_output() {
        let (path, rx) = start_listener();
        let cli = Cli::parse_from(["ox-rt", "done", "--force", "pass:7"]);
        run(cli, &path).expect("run");
        assert_eq!(rx.recv().unwrap(), "done pass:7\n");
    }

    #[test]
    fn done_with_force_no_output_sends_bare_done() {
        let (path, rx) = start_listener();
        let cli = Cli::parse_from(["ox-rt", "done", "--force"]);
        run(cli, &path).expect("run");
        assert_eq!(rx.recv().unwrap(), "done\n");
    }

    #[test]
    fn artifact_with_inline_content_is_base64_encoded() {
        let (path, rx) = start_listener();
        let cli = Cli::parse_from(["ox-rt", "artifact", "proposal", "Hello"]);
        run(cli, &path).expect("run");
        // "Hello" -> "SGVsbG8="
        assert_eq!(rx.recv().unwrap(), "artifact proposal SGVsbG8=\n");
    }

    #[test]
    fn artifact_done_sends_name() {
        let (path, rx) = start_listener();
        let cli = Cli::parse_from(["ox-rt", "artifact-done", "proposal"]);
        run(cli, &path).expect("run");
        assert_eq!(rx.recv().unwrap(), "artifact-done proposal\n");
    }

    // ── preflight_done tests ─────────────────────────────────────────
    //
    // These build small throwaway git repos in temp directories and drive
    // them through the scenarios preflight_done has to handle. We use a
    // bare "origin" repo next to each work repo so pushes, fetches, and
    // origin/<branch> lookups actually exercise the real code paths.

    use std::process::Command as ProcCommand;

    fn tmp_root() -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "ox-rt-preflight-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn run_git(dir: &Path, args: &[&str]) {
        let out = ProcCommand::new("git")
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@test")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@test")
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?} failed in {dir:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Create a repo pair: bare origin + work repo with an initial commit
    /// on `main` already pushed to origin. Returns the work repo path.
    fn init_repo_with_origin(root: &Path) -> std::path::PathBuf {
        let origin = root.join("origin.git");
        let work = root.join("work");
        std::fs::create_dir_all(&origin).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        run_git(&origin, &["init", "--bare", "-b", "main"]);
        run_git(&work, &["init", "-b", "main"]);
        run_git(&work, &["config", "user.email", "test@test"]);
        run_git(&work, &["config", "user.name", "test"]);
        run_git(
            &work,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        );
        std::fs::write(work.join("README"), "hello\n").unwrap();
        run_git(&work, &["add", "README"]);
        run_git(&work, &["commit", "-m", "initial"]);
        run_git(&work, &["push", "origin", "main"]);
        work
    }

    #[test]
    fn preflight_passes_outside_git_repo() {
        let root = tmp_root();
        // No git init — just a plain directory.
        assert!(preflight_done(&root).is_ok());
    }

    #[test]
    fn preflight_passes_on_main_even_if_dirty() {
        let root = tmp_root();
        let work = init_repo_with_origin(&root);
        // Make the tree dirty.
        std::fs::write(work.join("scratch"), "junk").unwrap();
        assert!(preflight_done(&work).is_ok(), "should pass on main");
    }

    #[test]
    fn preflight_passes_for_clean_pushed_branch_ahead_of_main() {
        let root = tmp_root();
        let work = init_repo_with_origin(&root);
        run_git(&work, &["checkout", "-b", "feature"]);
        std::fs::write(work.join("f"), "work\n").unwrap();
        run_git(&work, &["add", "f"]);
        run_git(&work, &["commit", "-m", "work"]);
        run_git(&work, &["push", "origin", "feature"]);
        assert!(
            preflight_done(&work).is_ok(),
            "should pass for clean, pushed, ahead-of-main branch"
        );
    }

    #[test]
    fn preflight_fails_on_dirty_feature_branch() {
        let root = tmp_root();
        let work = init_repo_with_origin(&root);
        run_git(&work, &["checkout", "-b", "feature"]);
        std::fs::write(work.join("f"), "work\n").unwrap();
        run_git(&work, &["add", "f"]);
        run_git(&work, &["commit", "-m", "work"]);
        run_git(&work, &["push", "origin", "feature"]);
        // Dirty the tree post-push.
        std::fs::write(work.join("g"), "uncommitted\n").unwrap();
        let err = preflight_done(&work).unwrap_err();
        assert!(err.contains("working tree is dirty"), "got: {err}");
    }

    #[test]
    fn preflight_fails_when_branch_never_pushed() {
        let root = tmp_root();
        let work = init_repo_with_origin(&root);
        run_git(&work, &["checkout", "-b", "feature"]);
        std::fs::write(work.join("f"), "work\n").unwrap();
        run_git(&work, &["add", "f"]);
        run_git(&work, &["commit", "-m", "work"]);
        // No push.
        let err = preflight_done(&work).unwrap_err();
        assert!(
            err.contains("origin/feature does not exist"),
            "got: {err}"
        );
    }

    #[test]
    fn preflight_fails_when_local_ahead_of_remote() {
        let root = tmp_root();
        let work = init_repo_with_origin(&root);
        run_git(&work, &["checkout", "-b", "feature"]);
        std::fs::write(work.join("f1"), "first\n").unwrap();
        run_git(&work, &["add", "f1"]);
        run_git(&work, &["commit", "-m", "first"]);
        run_git(&work, &["push", "origin", "feature"]);
        // Local commit not pushed.
        std::fs::write(work.join("f2"), "second\n").unwrap();
        run_git(&work, &["add", "f2"]);
        run_git(&work, &["commit", "-m", "second"]);
        let err = preflight_done(&work).unwrap_err();
        assert!(err.contains("ahead of origin/feature"), "got: {err}");
    }

    #[test]
    fn preflight_fails_when_branch_has_no_commits_ahead_of_main() {
        let root = tmp_root();
        let work = init_repo_with_origin(&root);
        // Branch from main with no new commits, then push.
        run_git(&work, &["checkout", "-b", "feature"]);
        run_git(&work, &["push", "origin", "feature"]);
        let err = preflight_done(&work).unwrap_err();
        assert!(
            err.contains("no commits ahead of origin/main"),
            "got: {err}"
        );
    }

    #[test]
    fn error_response_surfaces_as_failure() {
        let (path, _rx) = start_listener_with_response(b"error: no such step\n");
        let cli = Cli::parse_from(["ox-rt", "metric", "x", "1"]);
        let err = run(cli, &path).expect_err("should fail");
        assert!(
            err.to_string().contains("no such step"),
            "got: {err}"
        );
    }
}
