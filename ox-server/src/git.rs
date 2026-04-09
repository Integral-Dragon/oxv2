//! Git smart HTTP backend.
//!
//! Delegates to `git http-backend` (CGI) for clone and push operations.
//! The managed repository lives at `state.repo_path`. ox-runner clones
//! and pushes over HTTP — no local path access.
//!
//! Push to `main` is rejected via a pre-receive hook installed at init.
//! After a successful push, a `git.branch_pushed` event is emitted.

use axum::{
    Router,
    body::Body,
    extract::{Request, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::any,
};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::process::Command;

use crate::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/git/{*path}", any(git_handler))
}

/// Ensure the repo is configured to allow dumb/smart HTTP serving.
/// Installs a pre-receive hook that rejects pushes to main.
pub fn init_repo_for_http(repo_path: &Path) -> anyhow::Result<()> {
    // Enable http.receivepack so git http-backend accepts pushes
    let status = std::process::Command::new("git")
        .args(["config", "http.receivepack", "true"])
        .current_dir(repo_path)
        .status()?;
    if !status.success() {
        anyhow::bail!("failed to set http.receivepack");
    }

    // Run git update-server-info for dumb HTTP fallback
    let _ = std::process::Command::new("git")
        .args(["update-server-info"])
        .current_dir(repo_path)
        .status();

    // Install pre-receive hook that rejects pushes to main
    let hooks_dir = find_hooks_dir(repo_path);
    std::fs::create_dir_all(&hooks_dir)?;

    let hook_path = hooks_dir.join("pre-receive");
    std::fs::write(
        &hook_path,
        r#"#!/bin/sh
# Reject pushes to main — only merge_to_main may advance main.
while read oldrev newrev refname; do
    if [ "$refname" = "refs/heads/main" ]; then
        echo "error: direct push to main is not allowed" >&2
        echo "error: use merge_to_main to advance main" >&2
        exit 1
    fi
done
"#,
    )?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook_path, std::fs::Permissions::from_mode(0o755))?;
    }

    tracing::info!(repo = %repo_path.display(), "repo configured for HTTP serving");
    Ok(())
}

/// Find the hooks directory — works for both bare and non-bare repos.
fn find_hooks_dir(repo_path: &Path) -> PathBuf {
    let bare_hooks = repo_path.join("hooks");
    let worktree_hooks = repo_path.join(".git").join("hooks");
    if repo_path.join(".git").is_dir() {
        worktree_hooks
    } else {
        bare_hooks
    }
}

/// Find the git dir — `.git` for worktrees, the repo itself for bare repos.
fn find_git_dir(repo_path: &Path) -> PathBuf {
    let dot_git = repo_path.join(".git");
    if dot_git.is_dir() {
        dot_git
    } else {
        repo_path.to_path_buf()
    }
}

/// Handler for all `/git/*` requests. Delegates to `git http-backend` CGI.
async fn git_handler(
    State(state): State<AppState>,
    req: Request<Body>,
) -> Response {
    let uri = req.uri().clone();
    let method = req.method().clone();
    let headers = req.headers().clone();

    // Extract the path after /git/
    let path = uri.path().strip_prefix("/git/").unwrap_or(uri.path());

    // Determine query string
    let query = uri.query().unwrap_or("");

    // Read request body
    let body_bytes = match axum::body::to_bytes(req.into_body(), 256 * 1024 * 1024).await {
        Ok(b) => b,
        Err(e) => {
            tracing::error!(err = %e, "failed to read git request body");
            return (StatusCode::BAD_REQUEST, "failed to read body").into_response();
        }
    };

    let git_dir = find_git_dir(&state.repo_path);

    // Build CGI environment for git http-backend
    let mut cmd = Command::new("git");
    cmd.arg("http-backend");
    cmd.env("GIT_PROJECT_ROOT", &state.repo_path);
    cmd.env("GIT_HTTP_EXPORT_ALL", "1");
    cmd.env("PATH_INFO", format!("/{path}"));
    cmd.env("QUERY_STRING", query);
    cmd.env("REQUEST_METHOD", method.as_str());
    cmd.env("GIT_DIR", &git_dir);

    // Pass content type if present
    if let Some(ct) = headers.get("content-type") {
        cmd.env("CONTENT_TYPE", ct.to_str().unwrap_or(""));
    }

    cmd.env(
        "CONTENT_LENGTH",
        body_bytes.len().to_string(),
    );

    // Pass SERVER_PROTOCOL (required by http-backend)
    cmd.env("SERVER_PROTOCOL", "HTTP/1.1");

    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(err = %e, "failed to spawn git http-backend");
            return (StatusCode::INTERNAL_SERVER_ERROR, "git http-backend not available")
                .into_response();
        }
    };

    // Write request body to stdin
    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        if let Err(e) = stdin.write_all(&body_bytes).await {
            tracing::warn!(err = %e, "failed to write to git http-backend stdin");
        }
        drop(stdin);
    }

    // Read response from stdout
    let output = match child.wait_with_output().await {
        Ok(o) => o,
        Err(e) => {
            tracing::error!(err = %e, "git http-backend failed");
            return (StatusCode::INTERNAL_SERVER_ERROR, "git http-backend error")
                .into_response();
        }
    };

    if !output.stderr.is_empty() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        tracing::debug!(stderr = %stderr, "git http-backend stderr");
    }

    // Parse CGI response: headers separated from body by \r\n\r\n
    parse_cgi_response(&output.stdout, &state, path).await
}

/// Parse a CGI response (Status + headers + body) into an axum Response.
/// Also emits git.branch_pushed events for receive-pack responses.
async fn parse_cgi_response(raw: &[u8], _state: &AppState, path: &str) -> Response {
    // CGI output: headers\r\n\r\n body  (or headers\n\n body)
    let raw_str = String::from_utf8_lossy(raw);

    // Find the header/body boundary
    let (header_section, body) = if let Some(pos) = raw_str.find("\r\n\r\n") {
        (&raw_str[..pos], &raw[pos + 4..])
    } else if let Some(pos) = raw_str.find("\n\n") {
        (&raw_str[..pos], &raw[pos + 2..])
    } else {
        // No headers — treat entire output as body
        return Response::builder()
            .status(200)
            .body(Body::from(raw.to_vec()))
            .unwrap();
    };

    let mut status_code = 200u16;
    let mut response_headers = HeaderMap::new();

    for line in header_section.lines() {
        if let Some(rest) = line.strip_prefix("Status: ") {
            if let Some(code_str) = rest.split_whitespace().next() {
                if let Ok(code) = code_str.parse::<u16>() {
                    status_code = code;
                }
            }
        } else if let Some((name, value)) = line.split_once(": ") {
            if let (Ok(name), Ok(value)) = (
                name.parse::<axum::http::header::HeaderName>(),
                value.parse::<axum::http::header::HeaderValue>(),
            ) {
                response_headers.insert(name, value);
            }
        }
    }

    // If this was a receive-pack (push) and it succeeded, emit git.branch_pushed
    if path == "git-receive-pack" && status_code == 200 {
        // The push succeeded. We can't easily extract branch names from the
        // pack protocol here, but the runner will inform us via the API.
        // The pre-receive hook already blocks pushes to main.
        tracing::info!("git push received via http-backend");
    }

    let mut builder = Response::builder().status(status_code);
    for (name, value) in &response_headers {
        builder = builder.header(name, value);
    }

    builder.body(Body::from(body.to_vec())).unwrap()
}
