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

use crate::socket::{self, RuntimeCommand};

pub struct Runner {
    client: OxClient,
    server_url: String,
    environment: String,
    workspace_dir: PathBuf,
    runner_id: Option<RunnerId>,
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
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(10));
            loop {
                interval.tick().await;
                if let Err(e) = hb_client.heartbeat(&hb_id).await {
                    tracing::warn!(err = %e, "heartbeat failed");
                }
            }
        });

        // Subscribe to SSE
        let url = format!("{}/api/events/stream", self.server_url);
        let mut es = EventSource::get(&url);

        tracing::info!("subscribed to SSE, waiting for step assignments");

        loop {
            match es.next().await {
                Some(Ok(SseEvent::Message(msg))) => {
                    if msg.event == "step.dispatched" {
                        if let Err(e) = self.handle_dispatch(&msg.data).await {
                            tracing::error!(err = %e, "error handling step dispatch");
                        }
                    } else if msg.event == "runner.drained" {
                        if let Ok(d) = serde_json::from_str::<RunnerDrainedData>(&msg.data) {
                            if Some(&d.runner_id) == self.runner_id.as_ref() {
                                tracing::info!("received drain signal, exiting");
                                return Ok(());
                            }
                        }
                    }
                }
                Some(Ok(SseEvent::Open)) => {}
                Some(Err(reqwest_eventsource::Error::StreamEnded)) => {
                    tracing::warn!("SSE stream ended, reconnecting...");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    es = EventSource::get(&url);
                }
                Some(Err(e)) => {
                    tracing::warn!(err = %e, "SSE error, reconnecting...");
                    tokio::time::sleep(Duration::from_secs(1)).await;
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

        self.execute_step(assignment).await
    }

    async fn execute_step(&self, assignment: StepAssignment) -> Result<()> {
        let exec_id = &assignment.execution_id;
        let step = &assignment.step;
        let attempt = assignment.attempt;
        let start = Instant::now();

        // 1. Provision workspace
        let work_dir = self.workspace_dir.join("current");
        let tmp_dir = self.workspace_dir.join("tmp");
        std::fs::create_dir_all(&work_dir)?;
        std::fs::create_dir_all(&tmp_dir)?;

        // 2. Place files from resolved spec
        if let Some(ref resolved) = assignment.resolved {
            for file in &resolved.files {
                let target = work_dir.join(&file.to);
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&target, &file.content)?;
                // Set file mode if specified
                #[cfg(unix)]
                if file.mode != "0644" {
                    use std::os::unix::fs::PermissionsExt;
                    if let Ok(mode) = u32::from_str_radix(file.mode.trim_start_matches('0'), 8) {
                        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode))?;
                    }
                }
                tracing::debug!(file = %file.to, "placed file");
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

        // 4. Build command and env
        let (cmd_args, env_vars) = if let Some(ref resolved) = assignment.resolved {
            (resolved.command.clone(), resolved.env.clone())
        } else {
            // Fallback: just run a shell that waits
            (vec!["sh".into(), "-c".into(), "echo 'no resolved spec'; exit 1".into()], HashMap::new())
        };

        if cmd_args.is_empty() {
            // No command — report failure
            self.client
                .step_fail(exec_id, step, attempt, "no command in resolved spec")
                .await?;
            cleanup(&work_dir, &tmp_dir);
            return Ok(());
        }

        // 5. Spawn the runtime process
        let mut cmd = Command::new(&cmd_args[0]);
        cmd.args(&cmd_args[1..])
            .current_dir(&work_dir)
            .env("OX_SOCKET", &socket_path);

        for (k, v) in &env_vars {
            cmd.env(k, v);
        }

        tracing::info!(cmd = ?cmd_args, "spawning runtime process");

        let mut child = match cmd.stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(err = %e, "failed to spawn runtime");
                self.client
                    .step_fail(exec_id, step, attempt, &format!("spawn failed: {e}"))
                    .await?;
                cleanup(&work_dir, &tmp_dir);
                return Ok(());
            }
        };

        // 6. Wait for runtime: process socket commands until process exits
        let mut got_done = false;
        let mut output = String::new();

        // Spawn a task to drain socket commands
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

        // Wait for process to exit
        let status = child.wait().await?;
        tracing::info!(exit_code = status.code(), "runtime process exited");

        // Check if done was received
        if let Ok(o) = done_rx.try_recv() {
            got_done = true;
            output = o;
            tracing::info!(output = %output, "runtime called done");
        }

        socket_task.abort();

        // 7. Collect signals
        let duration = start.elapsed();
        let mut signals = vec![];

        if !got_done {
            signals.push("exited_silent".to_string());
        }
        if duration.as_secs() < 30 {
            signals.push("fast_exit".to_string());
        }

        // Check for no_commits if push was expected
        // (simplified — in real impl we'd check git log)
        if assignment.workspace.push {
            // TODO: check if HEAD advanced
        }

        // 8. Report done (if runtime called done)
        if got_done {
            self.client.step_done(exec_id, step, attempt, &output).await?;
        }

        // 9. Report signals
        self.client
            .step_signals(exec_id, step, attempt, signals.clone())
            .await?;

        // 10. Check signal failure rules
        let has_failure_signal = signals.iter().any(|s| {
            s == "exited_silent" || s == "no_commits" || s == "dirty_workspace"
        });

        if has_failure_signal {
            let error = signals
                .iter()
                .find(|s| *s == "exited_silent" || *s == "no_commits" || *s == "dirty_workspace")
                .map(|s| format!("signal:{s}"))
                .unwrap_or_default();
            tracing::warn!(exec = %exec_id, step = %step, error = %error, "step failed due to signal");
            self.client.step_fail(exec_id, step, attempt, &error).await?;
        } else {
            // 11. Confirm step
            let metrics = serde_json::json!({
                "runner": {
                    "duration_ms": duration.as_millis() as u64,
                }
            });
            self.client
                .step_confirm(exec_id, step, attempt, Some(metrics))
                .await?;
            tracing::info!(exec = %exec_id, step = %step, "step confirmed");
        }

        // 12. Cleanup
        socket_handle.abort();
        cleanup(&work_dir, &tmp_dir);

        Ok(())
    }
}

fn cleanup(work_dir: &Path, tmp_dir: &Path) {
    let _ = std::fs::remove_dir_all(work_dir);
    let _ = std::fs::remove_dir_all(tmp_dir);
}
