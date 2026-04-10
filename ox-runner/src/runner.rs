use anyhow::{Context, Result};
use futures_util::StreamExt;
use ox_core::client::OxClient;
use ox_core::events::*;
use ox_core::runtime::ResolvedStepSpec;
use ox_core::types::RunnerId;
use reqwest_eventsource::{Event as SseEvent, EventSource};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::process::Command;

use crate::proxy;
use crate::socket::{self, RuntimeCommand};

/// Shared state for heartbeat reporting.
#[derive(Debug, Clone, Default)]
struct HeartbeatState {
    execution_id: Option<String>,
    step: Option<String>,
    attempt: Option<u32>,
}

pub struct Runner {
    client: OxClient,
    server_url: String,
    environment: String,
    workspace_dir: PathBuf,
    runner_id: Option<RunnerId>,
    heartbeat_state: std::sync::Arc<std::sync::Mutex<HeartbeatState>>,
}

/// Parsed dispatch payload for the runner.
#[derive(Debug)]
struct StepAssignment {
    execution_id: String,
    step: String,
    attempt: u32,
    resolved: Option<ResolvedStepSpec>,
    workspace: WorkspaceSpec,
}

#[derive(Debug, serde::Deserialize)]
#[allow(dead_code)]
struct WorkspaceSpec {
    #[serde(default)]
    git_clone: bool,
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    push: bool,
    #[serde(default)]
    read_only: bool,
}

impl Runner {
    pub fn new(server_url: &str, environment: &str, workspace_dir: &str) -> Self {
        Self {
            client: OxClient::new(server_url),
            server_url: server_url.trim_end_matches('/').to_string(),
            environment: environment.to_string(),
            workspace_dir: PathBuf::from(workspace_dir),
            runner_id: None,
            heartbeat_state: std::sync::Arc::new(std::sync::Mutex::new(HeartbeatState::default())),
        }
    }

    pub async fn run(&mut self) -> Result<()> {
        // Register with ox-server
        let runner_id = self
            .client
            .register_runner(&self.environment, HashMap::new())
            .await
            .context("registering with ox-server")?;
        self.runner_id = Some(runner_id.clone());
        tracing::info!(runner = %runner_id, "registered with ox-server");

        // Start heartbeat background task
        let hb_client = OxClient::new(&self.server_url);
        let hb_id = runner_id.clone();
        let hb_state = self.heartbeat_state.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(10));
            loop {
                interval.tick().await;
                let state = hb_state.lock().unwrap().clone();
                if let Err(e) = hb_client
                    .heartbeat(&hb_id, state.execution_id.as_deref(), state.step.as_deref(), state.attempt)
                    .await
                {
                    tracing::warn!(err = %e, "heartbeat failed");
                }
            }
        });

        // Subscribe to SSE from current seq — runner only needs live events,
        // not historical replay. Old dispatches for recycled runner IDs must be ignored.
        let current_seq = self.client.status().await
            .map(|s| s.event_seq)
            .unwrap_or(0);
        let url = format!("{}/api/events/stream?last_event_id={}", self.server_url, current_seq);
        let mut es = EventSource::get(&url);

        tracing::info!("subscribed to SSE, waiting for step assignments");

        let mut backoff_secs: u64 = 1;
        const MAX_BACKOFF: u64 = 30;

        loop {
            match es.next().await {
                Some(Ok(SseEvent::Message(msg))) => {
                    if msg.event == "step.dispatched" {
                        if let Err(e) = self.handle_dispatch(&msg.data).await {
                            tracing::error!(err = %e, "error handling step dispatch");
                        }
                    } else if msg.event == "runner.drained"
                        && let Ok(d) = serde_json::from_str::<RunnerDrainedData>(&msg.data)
                            && Some(&d.runner_id) == self.runner_id.as_ref() {
                                tracing::info!("received drain signal, exiting");
                                return Ok(());
                            }
                }
                Some(Ok(SseEvent::Open)) => {
                    backoff_secs = 1;
                }
                Some(Err(reqwest_eventsource::Error::StreamEnded)) => {
                    tracing::warn!("SSE stream ended, reconnecting...");
                    es = EventSource::get(&url);
                }
                Some(Err(e)) => {
                    tracing::warn!(err = %e, backoff = backoff_secs, "SSE error, reconnecting...");
                    tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
                    backoff_secs = (backoff_secs * 2).min(MAX_BACKOFF);
                    es = EventSource::get(&url);
                }
                None => break,
            }
        }

        Ok(())
    }

    async fn handle_dispatch(&mut self, data: &str) -> Result<()> {
        // Parse the full SSE event envelope
        let envelope: EventEnvelope = serde_json::from_str(data)?;
        let dispatched: StepDispatchedData = serde_json::from_value(envelope.data)?;

        // Only handle steps assigned to us
        if Some(&dispatched.runner_id) != self.runner_id.as_ref() {
            return Ok(());
        }

        let workspace_spec: WorkspaceSpec =
            serde_json::from_value(dispatched.workspace).unwrap_or(WorkspaceSpec {
                git_clone: false,
                branch: None,
                push: false,
                read_only: false,
            });

        let resolved: Option<ResolvedStepSpec> = dispatched
            .runtime
            .get("resolved")
            .and_then(|v| serde_json::from_value(v.clone()).ok());

        let assignment = StepAssignment {
            execution_id: dispatched.execution_id.0,
            step: dispatched.step,
            attempt: dispatched.attempt,
            resolved,
            workspace: workspace_spec,
        };

        tracing::info!(
            exec = %assignment.execution_id,
            step = %assignment.step,
            attempt = assignment.attempt,
            "received step assignment"
        );

        let result = self.execute_step(assignment).await;
        // Clear heartbeat state — step is done (success or failure)
        {
            let mut hb = self.heartbeat_state.lock().unwrap();
            *hb = HeartbeatState::default();
        }
        result
    }

    async fn execute_step(&self, assignment: StepAssignment) -> Result<()> {
        let exec_id = &assignment.execution_id;
        let step = &assignment.step;
        let attempt = assignment.attempt;
        let start = Instant::now();

        // Update heartbeat state so the server knows what we're working on
        {
            let mut hb = self.heartbeat_state.lock().unwrap();
            hb.execution_id = Some(exec_id.clone());
            hb.step = Some(step.clone());
            hb.attempt = Some(attempt);
        }

        let work_dir = self.workspace_dir.join("current");
        // Use local filesystem for tmp (sockets don't work on 9p/virtiofs mounts)
        let tmp_dir = PathBuf::from("/tmp").join(format!("ox-step-{}-{}-{}", exec_id, step, attempt));

        // Clean any leftover workspace from a previous step
        let _ = std::fs::remove_dir_all(&work_dir);
        let _ = std::fs::remove_dir_all(&tmp_dir);
        std::fs::create_dir_all(&tmp_dir)?;

        // 1. Provision workspace via git clone from ox-server
        let clone_head = if assignment.workspace.git_clone {
            let git_url = format!("{}/git/", self.server_url);
            let branch = assignment
                .workspace
                .branch
                .as_deref()
                .unwrap_or("main");

            tracing::info!(url = %git_url, branch = %branch, "cloning workspace from ox-server");

            let clone_status = std::process::Command::new("git")
                .args(["clone", &git_url, "--branch", branch, "--single-branch"])
                .arg(&work_dir)
                .status();

            match clone_status {
                Ok(s) if s.success() => {
                    tracing::info!(branch = %branch, "workspace cloned");
                }
                Ok(s) => {
                    // Branch might not exist yet — clone main and create it
                    tracing::debug!(code = ?s.code(), "branch clone failed, trying main + checkout -b");

                    let clone2 = std::process::Command::new("git")
                        .args(["clone", &git_url])
                        .arg(&work_dir)
                        .status();

                    match clone2 {
                        Ok(s2) if s2.success() => {
                            // Create the branch
                            let checkout = std::process::Command::new("git")
                                .args(["checkout", "-b", branch])
                                .current_dir(&work_dir)
                                .status();
                            if let Err(e) = checkout {
                                tracing::warn!(err = %e, "failed to create branch {branch}");
                            }
                        }
                        _ => {
                            let err = "git clone from ox-server failed";
                            tracing::error!(err);
                            self.client
                                .step_fail(exec_id, step, attempt, err)
                                .await?;
                            cleanup(&work_dir, &tmp_dir);
                            return Ok(());
                        }
                    }
                }
                Err(e) => {
                    let err = format!("git clone failed: {e}");
                    tracing::error!(err = %err);
                    self.client
                        .step_fail(exec_id, step, attempt, &err)
                        .await?;
                    cleanup(&work_dir, &tmp_dir);
                    return Ok(());
                }
            }

            // Record HEAD before runtime runs (for no_commits detection)
            git_head(&work_dir)
        } else {
            // No git clone — just create an empty workspace
            std::fs::create_dir_all(&work_dir)?;
            None
        };

        // 2. Place files from resolved spec
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
        let work_dir_str = work_dir.to_string_lossy().to_string();
        let tmp_dir_str = tmp_dir.to_string_lossy().to_string();

        if let Some(ref resolved) = assignment.resolved {
            for file in &resolved.files {
                let target = resolve_file_path(&file.to, &work_dir_str, &tmp_dir_str, &home);
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&target, &file.content)?;
                #[cfg(unix)]
                if file.mode != "0644" {
                    use std::os::unix::fs::PermissionsExt;
                    if let Ok(mode) = u32::from_str_radix(file.mode.trim_start_matches('0'), 8) {
                        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode))?;
                    }
                }
                tracing::debug!(file = %target.display(), "placed file");
            }
        }

        // 3. Create unix socket
        let socket_path = tmp_dir.join(format!(
            "ox-{}-{}-{}-{}.sock",
            self.runner_id.as_ref().map(|r| r.0.as_str()).unwrap_or("?"),
            exec_id,
            step,
            attempt
        ));
        let (mut cmd_rx, socket_handle) = socket::start_socket_server(&socket_path)?;
        tracing::debug!(socket = %socket_path.display(), "socket server started");

        // 4. Start API proxies
        let mut proxy_handles = vec![];
        let mut proxy_env_overrides = HashMap::new();
        if let Some(ref resolved) = assignment.resolved {
            for proxy_def in &resolved.proxy {
                match proxy::start_proxy(
                    proxy_def.target.clone(),
                    proxy_def.provider.clone(),
                )
                .await
                {
                    Ok(handle) => {
                        let proxy_url = format!("http://{}", handle.local_addr);
                        tracing::info!(
                            env = %proxy_def.env,
                            proxy_url = %proxy_url,
                            provider = %proxy_def.provider,
                            "started API proxy"
                        );
                        proxy_env_overrides.insert(proxy_def.env.clone(), proxy_url);
                        proxy_handles.push(handle);
                    }
                    Err(e) => {
                        tracing::warn!(err = %e, "failed to start API proxy");
                    }
                }
            }
        }

        // 5. Build command and env
        let (cmd_args, mut env_vars) = if let Some(ref resolved) = assignment.resolved {
            (resolved.command.clone(), resolved.env.clone())
        } else {
            (vec!["sh".into(), "-c".into(), "echo 'no resolved spec'; exit 1".into()], HashMap::new())
        };

        // Apply proxy env overrides
        for (k, v) in proxy_env_overrides {
            env_vars.insert(k, v);
        }

        // Resolve runner-local placeholders in command args
        let cmd_args: Vec<String> = cmd_args
            .iter()
            .map(|a| resolve_placeholders(a, &work_dir_str, &tmp_dir_str, &home))
            .collect();

        if cmd_args.is_empty() {
            self.client
                .step_fail(exec_id, step, attempt, "no command in resolved spec")
                .await?;
            cleanup(&work_dir, &tmp_dir);
            return Ok(());
        }

        // 6. Spawn the runtime process
        let mut cmd = Command::new(&cmd_args[0]);
        cmd.args(&cmd_args[1..])
            .current_dir(&work_dir)
            .env("OX_SOCKET", &socket_path)
            .env("OX_TASK_ID", assignment.execution_id.split('-').next().unwrap_or(""));

        // Add ox bin directory to PATH so ox-rt is available
        let current_path = std::env::var("PATH").unwrap_or_default();
        if let Ok(exe) = std::env::current_exe() {
            // ox-runner is at target/debug/ox-runner, ox-rt is at bin/ox-rt
            // Go up to the project root and find bin/
            if let Some(project_root) = exe.parent().and_then(|p| p.parent()).and_then(|p| p.parent()) {
                let bin_dir = project_root.join("bin");
                if bin_dir.join("ox-rt").exists() {
                    cmd.env("PATH", format!("{}:{}", bin_dir.display(), current_path));
                }
            }
        }

        for (k, v) in &env_vars {
            cmd.env(k, v);
        }

        tracing::info!(cmd = ?cmd_args, "spawning runtime process");

        // Set up log file for stdout/stderr capture
        let log_file_path = tmp_dir.join("step.log");
        let log_file = std::fs::File::create(&log_file_path)?;
        let log_file_err = log_file.try_clone()?;

        let mut child = match cmd
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::from(log_file))
            .stderr(std::process::Stdio::from(log_file_err))
            .spawn()
        {
            Ok(c) => {
                // Signal that the runtime process is now running
                if let Err(e) = self.client.step_running(exec_id, step, attempt).await {
                    tracing::warn!(err = %e, "failed to report step running");
                }
                c
            }
            Err(e) => {
                tracing::error!(err = %e, "failed to spawn runtime");
                self.client
                    .step_fail(exec_id, step, attempt, &format!("spawn failed: {e}"))
                    .await?;
                cleanup(&work_dir, &tmp_dir);
                return Ok(());
            }
        };

        // Spawn log pusher — tails the log file and ships chunks to ox-server
        let log_pos = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let log_pusher_client = OxClient::new(&self.server_url);
        let log_exec_id = exec_id.to_string();
        let log_step = step.to_string();
        let log_path = log_file_path.clone();
        let log_pos_clone = log_pos.clone();
        let log_pusher = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(5));
            interval.tick().await;
            loop {
                interval.tick().await;
                let pos = log_pos_clone.load(std::sync::atomic::Ordering::Relaxed);
                let new_pos = flush_log_chunk(
                    &log_pusher_client,
                    &log_exec_id,
                    &log_step,
                    attempt,
                    &log_path,
                    pos,
                )
                .await;
                log_pos_clone.store(new_pos, std::sync::atomic::Ordering::Relaxed);
            }
        });

        // 7. Wait for runtime: process socket commands until process exits
        let mut got_done = false;
        let mut output = String::new();

        let (done_tx, mut done_rx) = tokio::sync::oneshot::channel::<String>();
        let socket_task = tokio::spawn(async move {
            let mut done_tx = Some(done_tx);
            while let Some(cmd) = cmd_rx.recv().await {
                match cmd {
                    RuntimeCommand::Done { output } => {
                        if let Some(tx) = done_tx.take() {
                            let _ = tx.send(output);
                        }
                    }
                    RuntimeCommand::Metric { name, value } => {
                        tracing::debug!(metric = %name, value = %value, "runtime metric");
                    }
                    RuntimeCommand::Artifact { name, .. } => {
                        tracing::debug!(artifact = %name, "runtime artifact chunk");
                    }
                    RuntimeCommand::ArtifactDone { name } => {
                        tracing::debug!(artifact = %name, "runtime artifact closed");
                    }
                }
            }
        });

        let status = child.wait().await?;
        tracing::info!(exit_code = status.code(), "runtime process exited");

        // Final log flush — send anything written since the last periodic push
        log_pusher.abort();
        let final_pos = log_pos.load(std::sync::atomic::Ordering::Relaxed);
        flush_log_chunk(&self.client, exec_id, step, attempt, &log_file_path, final_pos).await;

        if let Ok(o) = done_rx.try_recv() {
            got_done = true;
            output = o;
            tracing::info!(output = %output, "runtime called done");
        }

        socket_task.abort();

        // 8. Collect signals
        let duration = start.elapsed();
        let mut signals = vec![];

        // If the process exited 0 but never called ox-rt done, treat as implicit done.
        // Output is empty — the workflow engine will fall through to the next step
        // by declaration order, or the step's transitions can match on "".
        if !got_done && status.success() {
            tracing::info!("runtime exited 0 without calling ox-rt done, inferring done");
            got_done = true;
            output = String::new();
        }

        if !got_done {
            signals.push("exited_silent".to_string());
        }
        if duration.as_secs() < 30 {
            signals.push("fast_exit".to_string());
        }

        // Check for no_commits if push was expected
        if assignment.workspace.push {
            let post_head = git_head(&work_dir);
            if clone_head.is_some() && clone_head == post_head {
                signals.push("no_commits".to_string());
            }
        }

        // Check for dirty workspace
        if assignment.workspace.git_clone && is_workspace_dirty(&work_dir) {
            signals.push("dirty_workspace".to_string());
        }

        // 9. Report done (if runtime called done)
        if got_done {
            self.client.step_done(exec_id, step, attempt, &output).await?;
        }

        // 10. Report signals
        self.client
            .step_signals(exec_id, step, attempt, signals.clone())
            .await?;

        // 11. Check signal failure rules
        // Only exited_silent is a hard failure — the agent never signaled
        // completion. Workspace signals (no_commits, dirty_workspace) are
        // informational: the agent manages its own git flow and calls
        // ox-rt done when it's finished.
        let has_failure_signal = signals.contains(&"exited_silent".to_string());

        if has_failure_signal {
            let error = "signal:exited_silent".to_string();
            tracing::warn!(exec = %exec_id, step = %step, error = %error, "step failed due to signal");
            self.client.step_fail(exec_id, step, attempt, &error).await?;
        } else {
            // 12. Confirm step
            let proxy_metrics = proxy::collect_proxy_metrics(&proxy_handles);

            let metrics = serde_json::json!({
                "runner": {
                    "duration_ms": duration.as_millis() as u64,
                    "exit_code": status.code(),
                },
                "proxy": proxy_metrics,
            });
            self.client
                .step_confirm(exec_id, step, attempt, Some(metrics))
                .await?;
            tracing::info!(exec = %exec_id, step = %step, "step confirmed");
        }

        // 14. Cleanup: stop proxies, socket, and workspace
        for handle in proxy_handles {
            handle.task.abort();
        }
        socket_handle.abort();
        cleanup(&work_dir, &tmp_dir);

        Ok(())
    }
}

/// Get the current HEAD commit hash in a workspace.
fn git_head(work_dir: &Path) -> Option<String> {
    std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(work_dir)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// Check if the workspace has uncommitted changes.
fn is_workspace_dirty(work_dir: &Path) -> bool {
    std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(work_dir)
        .output()
        .ok()
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false)
}

fn cleanup(work_dir: &Path, tmp_dir: &Path) {
    let _ = std::fs::remove_dir_all(work_dir);
    let _ = std::fs::remove_dir_all(tmp_dir);
}

/// Resolve {workspace}, {tmp_dir}, {home} placeholders in a string.
fn resolve_placeholders(s: &str, work_dir: &str, tmp_dir: &str, home: &str) -> String {
    s.replace("{workspace}", work_dir)
        .replace("{tmp_dir}", tmp_dir)
        .replace("{home}", home)
}

/// Resolve a file destination path. Bare names (no placeholders) go to tmp_dir.
fn resolve_file_path(to: &str, work_dir: &str, tmp_dir: &str, home: &str) -> PathBuf {
    let resolved = resolve_placeholders(to, work_dir, tmp_dir, home);
    let path = Path::new(&resolved);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        // Bare relative paths go in tmp_dir
        PathBuf::from(tmp_dir).join(&resolved)
    }
}

/// Read new data from a log file starting at `pos` and push it to ox-server.
/// Returns the new position.
async fn flush_log_chunk(
    client: &OxClient,
    execution_id: &str,
    step: &str,
    attempt: u32,
    log_path: &Path,
    pos: u64,
) -> u64 {
    let contents = match std::fs::read(log_path) {
        Ok(c) => c,
        Err(_) => return pos,
    };

    let file_len = contents.len() as u64;
    // Handle truncation
    let pos = if file_len < pos { 0 } else { pos };

    if file_len <= pos {
        return pos;
    }

    let new_data = &contents[pos as usize..];
    let chunk = String::from_utf8_lossy(new_data);

    match client.push_log_chunk(execution_id, step, attempt, &chunk).await {
        Ok(()) => file_len,
        Err(_) => pos, // retry next tick
    }
}
