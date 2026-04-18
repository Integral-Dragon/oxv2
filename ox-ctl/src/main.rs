mod output;
mod up;

use anyhow::Result;
use clap::{Parser, Subcommand};
use ox_core::client::OxClient;

#[derive(Parser)]
#[command(name = "ox-ctl")]
struct Cli {
    /// ox-server URL.
    #[arg(long, env = "OX_SERVER", default_value = "http://localhost:4840", global = true)]
    server: String,

    /// Output as JSON.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Show server status.
    Status,

    /// Manage executions.
    Exec {
        #[command(subcommand)]
        command: ExecCommands,
    },

    /// Manage runners.
    Runners {
        #[command(subcommand)]
        command: RunnerCommands,
    },

    /// Manage secrets.
    Secrets {
        #[command(subcommand)]
        command: SecretCommands,
    },

    /// List loaded workflows.
    Workflows,

    /// Tail the event stream.
    Events {
        /// Start from this sequence number.
        #[arg(long)]
        since: Option<u64>,

        /// Filter by event type prefix.
        #[arg(long, name = "type")]
        type_filter: Option<String>,
    },

    /// Trigger a workflow for a cx node.
    Trigger {
        /// cx node ID.
        node_id: String,

        /// Bypass dedup check.
        #[arg(long)]
        force: bool,
    },

    /// Reload configuration from disk.
    Reload,

    /// Config management.
    Config {
        #[command(subcommand)]
        command: ConfigCommands,
    },

    /// Start the local ensemble (server + herder + seguro runners)
    /// for the current directory.
    Up {
        /// Number of seguro runners to launch.
        #[arg(long, env = "OX_RUNNERS", default_value = "2")]
        runners: usize,
        /// Server port (bound on localhost).
        #[arg(long, env = "OX_PORT", default_value = "4840")]
        port: u16,
    },

    /// Stop the local ensemble started with `ox-ctl up`.
    Down,

    /// Wipe the local ensemble's database and logs.
    /// Requires that the ensemble is stopped.
    Reset,
}

#[derive(Subcommand)]
enum ExecCommands {
    /// List executions (most recent first, default 25).
    List {
        #[arg(long)]
        status: Option<String>,
        #[arg(long)]
        workflow: Option<String>,
        /// Max results to show.
        #[arg(long, short = 'n', default_value = "25")]
        limit: usize,
        /// Show all executions (no limit).
        #[arg(long)]
        all: bool,
    },
    /// Show execution detail.
    Show {
        /// Execution ID.
        id: String,
    },
    /// Cancel an execution.
    Cancel {
        /// Execution ID.
        id: String,
    },
    /// Retry an escalated execution. Cancels the old one (releasing
    /// the trigger-dedup lock) and creates a new one with the same
    /// workflow, vars, and origin. By default resumes from the
    /// escalated step; pass --from-start to restart the workflow.
    Retry {
        /// Execution ID of the escalated execution to retry.
        id: String,
        /// Restart at the workflow's first step instead of resuming
        /// from the escalated step.
        #[arg(long)]
        from_start: bool,
    },
    /// Attach to an interactive PTY step.
    Attach {
        /// Execution ID.
        id: String,
        /// Step name.
        step: String,
    },
    /// Show step logs.
    Logs {
        /// Execution ID.
        id: String,
        /// Step name.
        step: String,
        /// Attempt number (defaults to most recent).
        #[arg(long)]
        attempt: Option<u32>,
        /// Show last N lines.
        #[arg(long, short = 'n')]
        lines: Option<usize>,
        /// Follow log output (like tail -f).
        #[arg(long, short = 'f')]
        follow: bool,
        /// Pretty-print Claude or Codex stream-json logs.
        #[arg(long, short = 'p')]
        pretty: bool,
    },
}

#[derive(Subcommand)]
enum RunnerCommands {
    /// List runners.
    List,
    /// Drain a runner.
    Drain {
        /// Runner ID.
        id: String,
    },
}

#[derive(Subcommand)]
enum SecretCommands {
    /// List secret names.
    List,
    /// Set a secret.
    Set {
        /// Secret name.
        name: String,
        /// Secret value. If omitted, reads from stdin.
        #[arg(long)]
        value: Option<String>,
    },
    /// Delete a secret.
    Delete {
        /// Secret name.
        name: String,
    },
}

#[derive(Subcommand)]
enum ConfigCommands {
    /// Validate config files without applying (dry-run).
    Check,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let client = OxClient::new(&cli.server);
    let json = cli.json;

    match cli.command {
        Commands::Status => cmd_status(&client, json).await,
        Commands::Exec { command } => match command {
            ExecCommands::List { status, workflow, limit, all } => {
                cmd_exec_list(&client, json, status, workflow, limit, all).await
            }
            ExecCommands::Show { id } => cmd_exec_show(&client, json, &id).await,
            ExecCommands::Cancel { id } => cmd_exec_cancel(&client, &id).await,
            ExecCommands::Retry { id, from_start } => cmd_exec_retry(&client, &id, from_start).await,
            ExecCommands::Attach { id, step } => cmd_attach(&cli.server, &id, &step).await,
            ExecCommands::Logs { id, step, attempt, lines, follow, pretty } => {
                cmd_logs(&cli.server, &id, &step, attempt, lines, follow, pretty).await
            }
        },
        Commands::Runners { command } => match command {
            RunnerCommands::List => cmd_runners_list(&client, json).await,
            RunnerCommands::Drain { id } => cmd_runners_drain(&client, &id).await,
        },
        Commands::Secrets { command } => match command {
            SecretCommands::List => cmd_secrets_list(&client, json).await,
            SecretCommands::Set { name, value } => cmd_secrets_set(&client, &name, value).await,
            SecretCommands::Delete { name } => cmd_secrets_delete(&client, &name).await,
        },
        Commands::Workflows => cmd_workflows(&client, json).await,
        Commands::Events { since, type_filter } => {
            cmd_events(&cli.server, json, since, type_filter).await
        }
        Commands::Trigger { node_id, force } => cmd_trigger(&client, &node_id, force).await,
        Commands::Reload => cmd_reload(&client, json).await,
        Commands::Config { command } => match command {
            ConfigCommands::Check => cmd_config_check(&client, json).await,
        },
        Commands::Up { runners, port } => up::cmd_up(runners, port).await,
        Commands::Down => up::cmd_down(),
        Commands::Reset => up::cmd_reset(),
    }
}

// ── Watchers (status section) ───────────────────────────────────────

/// One row as returned by `GET /api/watchers`. Mirrors the server's
/// `WatcherCursorRow` — kept local so ox-ctl can render without
/// pulling a shared crate for the HTTP shape.
#[derive(Debug, Clone, serde::Deserialize)]
struct WatcherRow {
    source: String,
    #[serde(default)]
    cursor: Option<String>,
    updated_at: String,
    #[serde(default)]
    last_error: Option<String>,
}

/// Parse a `StepAttemptId` Display string into (exec_id, step, attempt).
/// Format is `"{exec_id}/{step}/{attempt}"` — see `ox-core/src/types.rs`.
/// Returns None if the input is not exactly three `/`-separated fields
/// with a numeric attempt. `rsplitn` is used so that an exec_id or step
/// containing a `/` would at worst leak into `exec_id`, not misalign
/// the attempt field.
/// One runner row in the `ox-ctl status` per-runner section. Built in
/// the CLI by joining `/api/state/pool` (runner + current_step) against
/// `GET /api/executions/:id` (to resolve `workflow`). `workflow`, `exec_id`,
/// `step`, `attempt` are all None for an idle/drained runner with no
/// current step.
#[derive(Debug, Clone, serde::Serialize)]
struct RunnerRow {
    id: String,
    status: String,
    workflow: Option<String>,
    exec_id: Option<String>,
    step: Option<String>,
    attempt: Option<u32>,
}

/// Render the per-runner section of `ox-ctl status`. Pure — takes the
/// joined rows and returns a string. Empty input renders an empty
/// string so the caller can decide whether to print a header.
fn format_runners_section(rows: &[RunnerRow]) -> String {
    if rows.is_empty() {
        return String::new();
    }

    let mut out = String::new();
    out.push_str(&format!(
        "  {:<12} {:<10} {:<20} {}\n",
        "ID", "STATUS", "WORKFLOW", "STEP"
    ));
    for row in rows {
        let workflow = row.workflow.as_deref().unwrap_or("-");
        let step = match (&row.exec_id, &row.step, row.attempt) {
            (Some(exec), Some(step), Some(attempt)) => format!("{exec}/{step}#{attempt}"),
            _ => "-".to_string(),
        };
        out.push_str(&format!(
            "  {:<12} {:<10} {:<20} {}\n",
            row.id, row.status, workflow, step
        ));
    }
    out
}

fn parse_step_attempt(s: &str) -> Option<(String, String, u32)> {
    let mut parts = s.rsplitn(3, '/');
    let attempt_str = parts.next()?;
    let step = parts.next()?;
    let exec_id = parts.next()?;
    if exec_id.is_empty() || step.is_empty() {
        return None;
    }
    let attempt: u32 = attempt_str.parse().ok()?;
    Some((exec_id.to_string(), step.to_string(), attempt))
}

/// Render the watchers section of `ox-ctl status`. Pure — takes the
/// rows as input and returns the string to print (with a trailing
/// newline per line). Empty input renders an empty string so the
/// caller can decide whether to print a header.
fn format_watchers_section(rows: &[WatcherRow]) -> String {
    if rows.is_empty() {
        return String::new();
    }

    let mut out = String::new();
    out.push_str(&format!(
        "  {:<10} {:<22} {:<16} {}\n",
        "SOURCE", "LAST INGEST", "CURSOR", "STATUS"
    ));
    for row in rows {
        let cursor_display = match row.cursor.as_deref() {
            Some(s) if !s.is_empty() => {
                let head: String = s.chars().take(12).collect();
                if s.chars().count() > 12 {
                    format!("{head}…")
                } else {
                    head
                }
            }
            _ => "-".to_string(),
        };
        let status = row
            .last_error
            .clone()
            .unwrap_or_else(|| "alive".to_string());
        out.push_str(&format!(
            "  {:<10} {:<22} {:<16} {}\n",
            row.source, row.updated_at, cursor_display, status
        ));
    }
    out
}

// ── Status ──────────────────────────────────────────────────────────

async fn cmd_status(client: &OxClient, json: bool) -> Result<()> {
    // If the current directory looks like it was started with `ox-ctl up`,
    // show the local pidfile state before talking to the server. This lets
    // `ox-ctl status` work as a one-stop "is my ensemble alive" check.
    if !json
        && let Ok(cwd) = std::env::current_dir() {
            let paths = up::RunPaths::for_repo(&cwd);
            if paths.pidfile.is_file()
                && let Ok(content) = std::fs::read_to_string(&paths.pidfile)
            {
                for entry in up::parse_pidfile(&content) {
                    let state = if up::is_running(entry.pid) {
                        "alive"
                    } else {
                        "dead "
                    };
                    println!("  {:<10} pid={}  {}", entry.name, entry.pid, state);
                }
                println!();
            }
        }

    let s = client.status().await?;
    // Best-effort fetch — a server that hasn't been restarted yet
    // won't know the route. Treat any failure as "no watchers to
    // render" rather than aborting the whole status command.
    let watchers_raw = client.list_watchers().await.ok();
    let watchers: Vec<WatcherRow> = watchers_raw
        .as_ref()
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();

    let runners = collect_runner_rows(client).await.unwrap_or_default();

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "status": s.status,
                "pool_size": s.pool_size,
                "pool_executing": s.pool_executing,
                "pool_idle": s.pool_idle,
                "executions_running": s.executions_running,
                "workflows_loaded": s.workflows_loaded,
                "event_seq": s.event_seq,
                "watchers": watchers_raw.unwrap_or(serde_json::json!([])),
                "runners": runners,
            }))?
        );
    } else {
        println!("ox-server   {}   seq {}", s.status, s.event_seq);
        println!(
            "pool        {} runners ({} executing, {} idle)",
            s.pool_size, s.pool_executing, s.pool_idle
        );
        println!("executions  {} running", s.executions_running);
        println!("workflows   {} loaded", s.workflows_loaded);
        let section = format_runners_section(&runners);
        if !section.is_empty() {
            println!();
            println!("runners");
            print!("{section}");
        }
        let section = format_watchers_section(&watchers);
        if !section.is_empty() {
            println!();
            println!("watchers");
            print!("{section}");
        }
    }
    Ok(())
}

/// Fetch `/api/state/pool`, parse each runner, and for busy runners
/// resolve the workflow name via `GET /api/executions/:id`. Returns
/// an empty vec on any error (old server, network blip) — `cmd_status`
/// should still print the aggregate counts in that case.
async fn collect_runner_rows(client: &OxClient) -> Result<Vec<RunnerRow>> {
    let pool: serde_json::Value = reqwest::Client::new()
        .get(format!("{}/api/state/pool", client.base_url()))
        .send()
        .await?
        .json()
        .await?;

    let runners = pool
        .get("runners")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    // Gather unique execution ids that need a workflow lookup, then
    // fetch each at most once.
    let mut wanted: Vec<String> = Vec::new();
    for r in &runners {
        if let Some(step) = r.get("current_step").and_then(|v| v.as_str())
            && let Some((exec_id, _, _)) = parse_step_attempt(step)
            && !wanted.contains(&exec_id)
        {
            wanted.push(exec_id);
        }
    }
    let mut workflows: std::collections::HashMap<String, String> = Default::default();
    for exec_id in &wanted {
        if let Ok(detail) = client.get_execution(exec_id).await {
            workflows.insert(exec_id.clone(), detail.workflow);
        }
    }

    let mut rows = Vec::with_capacity(runners.len());
    for r in &runners {
        let id = r.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let status = r.get("status").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let (exec_id, step, attempt) = r
            .get("current_step")
            .and_then(|v| v.as_str())
            .and_then(parse_step_attempt)
            .map(|(e, s, a)| (Some(e), Some(s), Some(a)))
            .unwrap_or((None, None, None));
        let workflow = exec_id.as_ref().and_then(|e| workflows.get(e).cloned());
        rows.push(RunnerRow { id, status, workflow, exec_id, step, attempt });
    }
    Ok(rows)
}

// ── Executions ──────────────────────────────────────────────────────

async fn cmd_exec_list(
    client: &OxClient,
    json: bool,
    status: Option<String>,
    workflow: Option<String>,
    limit: usize,
    all: bool,
) -> Result<()> {
    let filter = ox_core::client::ListExecutionsFilter {
        status,
        workflow,
        limit: if all { None } else { Some(limit) },
        offset: None,
    };
    let resp = client.list_executions(filter).await?;

    if json {
        println!("{}", serde_json::to_string_pretty(&resp)?);
    } else {
        let execs = resp.get("executions").and_then(|v| v.as_array());
        let total = resp.get("total").and_then(|v| v.as_u64()).unwrap_or(0);

        println!(
            "{:<22} {:<16} {:<26} {:<14} {:<12} {:<20}",
            "ID", "WORKFLOW", "ORIGIN", "STEP", "STATUS", "CREATED"
        );
        if let Some(execs) = execs {
            for e in execs {
                let status = e.get("status").and_then(|v| v.as_str()).unwrap_or("-");
                let step = e.get("current_step").and_then(|v| v.as_str()).unwrap_or("-");
                let display_status = if status == "running" && step == "-" {
                    "pending"
                } else {
                    status
                };
                let created = e.get("created_at")
                    .and_then(|v| v.as_str())
                    .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                    .map(|dt| dt.format("%Y-%m-%d %H:%M:%S").to_string())
                    .unwrap_or_else(|| "-".into());
                let origin_str = e
                    .get("origin")
                    .and_then(|v| serde_json::from_value::<ox_core::events::ExecutionOrigin>(v.clone()).ok())
                    .map(|o| format_origin(&o))
                    .unwrap_or_else(|| "-".into());
                println!(
                    "{:<22} {:<16} {:<26} {:<14} {:<12} {:<20}",
                    e.get("id").and_then(|v| v.as_str()).unwrap_or("-"),
                    e.get("workflow").and_then(|v| v.as_str()).unwrap_or("-"),
                    origin_str,
                    step,
                    display_status,
                    created,
                );
            }
            let shown = execs.len() as u64;
            if shown < total {
                println!("\n{shown} of {total} total (use --all or -n to see more)");
            } else {
                println!("\n{total} total");
            }
        }
    }
    Ok(())
}

/// Render an `ExecutionOrigin` for display. Truncates with an ellipsis if
/// longer than `ORIGIN_DISPLAY_WIDTH` characters. Pure — all variants in
/// one place.
const ORIGIN_DISPLAY_WIDTH: usize = 24;

fn format_origin(origin: &ox_core::events::ExecutionOrigin) -> String {
    use ox_core::events::ExecutionOrigin::*;
    let raw = match origin {
        Event {
            source, subject_id, ..
        } => format!("{source}:{subject_id}"),
        Manual { user: Some(u) } => format!("manual:{u}"),
        Manual { user: None } => "manual".to_string(),
    };
    truncate_with_ellipsis(&raw, ORIGIN_DISPLAY_WIDTH)
}

fn truncate_with_ellipsis(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        // Leave room for the ellipsis char.
        let mut out: String = s.chars().take(max - 1).collect();
        out.push('…');
        out
    }
}

async fn cmd_exec_show(client: &OxClient, json: bool, id: &str) -> Result<()> {
    let exec = client.get_execution(id).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&serde_json::json!({
            "id": exec.id,
            "vars": exec.vars,
            "origin": exec.origin,
            "workflow": exec.workflow,
            "status": exec.status,
            "current_step": exec.current_step,
            "attempts": exec.attempts,
        }))?);
    } else {
        let created = chrono::DateTime::parse_from_rfc3339(&exec.created_at)
            .map(|dt| dt.format("%Y-%m-%d %H:%M:%S").to_string())
            .unwrap_or_else(|_| exec.created_at.clone());

        println!("Execution: {}", exec.id);
        println!("Origin:    {}", format_origin(&exec.origin));
        println!("Created:   {}", created);
        if !exec.vars.is_empty() {
            for (k, v) in &exec.vars {
                println!("  {k}: {v}");
            }
        }
        println!("Workflow:  {}", exec.workflow);
        println!("Status:    {}", exec.status);
        println!();
        println!(
            "{:<4} {:<14} {:<8} {:<10} {:<10} {:<10} {:<12} {:<16}",
            "#", "STEP", "ATTEMPT", "STATUS", "RUNNER", "DURATION", "OUTPUT", "TRANSITION"
        );
        for (i, a) in exec.attempts.iter().enumerate() {
            let duration = match (&a.started_at, &a.completed_at) {
                (started, Some(completed)) => {
                    if let (Ok(s), Ok(c)) = (
                        chrono::DateTime::parse_from_rfc3339(started),
                        chrono::DateTime::parse_from_rfc3339(completed),
                    ) {
                        let secs = (c - s).num_seconds();
                        if secs >= 60 {
                            format!("{}m{}s", secs / 60, secs % 60)
                        } else {
                            format!("{}s", secs)
                        }
                    } else {
                        "-".into()
                    }
                }
                (started, None) => {
                    // Still running — show elapsed
                    if let Ok(s) = chrono::DateTime::parse_from_rfc3339(started) {
                        let secs = (chrono::Utc::now() - s.with_timezone(&chrono::Utc)).num_seconds();
                        if secs >= 60 {
                            format!("{}m{}s…", secs / 60, secs % 60)
                        } else {
                            format!("{}s…", secs)
                        }
                    } else {
                        "-".into()
                    }
                }
            };
            println!(
                "{:<4} {:<14} {:<8} {:<10} {:<10} {:<10} {:<12} {:<16}",
                i + 1,
                a.step,
                a.attempt,
                a.status,
                a.runner_id.as_deref().unwrap_or("-"),
                duration,
                a.output.as_deref().unwrap_or("-"),
                a.transition
                    .as_deref()
                    .map(|t| format!("→ {t}"))
                    .unwrap_or_else(|| "-".into()),
            );
        }
    }
    Ok(())
}

async fn cmd_exec_cancel(client: &OxClient, id: &str) -> Result<()> {
    client.cancel_execution(id).await?;
    println!("Cancelled {id}");
    Ok(())
}

async fn cmd_exec_retry(client: &OxClient, id: &str, from_start: bool) -> Result<()> {
    let new_id = client.retry_execution(id, from_start).await?;
    let mode = if from_start { "from start" } else { "from failed step" };
    println!("Retry of {id} ({mode}) → {}", new_id.0);
    Ok(())
}

// ── Runners ─────────────────────────────────────────────────────────

async fn cmd_runners_list(client: &OxClient, json: bool) -> Result<()> {
    let pool: serde_json::Value = reqwest::Client::new()
        .get(format!("{}/api/state/pool", client.base_url()))
        .send()
        .await?
        .json()
        .await?;

    if json {
        println!("{}", serde_json::to_string_pretty(&pool)?);
    } else {
        let runners = pool
            .get("runners")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        println!(
            "{:<12} {:<12} {:<12} {:<24}",
            "ID", "ENVIRONMENT", "STATUS", "STEP"
        );
        for r in &runners {
            println!(
                "{:<12} {:<12} {:<12} {:<24}",
                r.get("id").and_then(|v| v.as_str()).unwrap_or("-"),
                r.get("environment").and_then(|v| v.as_str()).unwrap_or("-"),
                r.get("status").and_then(|v| v.as_str()).unwrap_or("-"),
                r.get("current_step").and_then(|v| v.as_str()).unwrap_or("-"),
            );
        }
    }
    Ok(())
}

async fn cmd_runners_drain(client: &OxClient, id: &str) -> Result<()> {
    client
        .drain_runner(&ox_core::types::RunnerId(id.to_string()))
        .await?;
    println!("Drained {id}");
    Ok(())
}

// ── Secrets ─────────────────────────────────────────────────────────

async fn cmd_secrets_list(client: &OxClient, json: bool) -> Result<()> {
    let secrets = client.list_secrets().await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&secrets
            .iter()
            .map(|s| serde_json::json!({"name": s.name}))
            .collect::<Vec<_>>())?);
    } else {
        println!("NAME");
        for s in &secrets {
            println!("{}", s.name);
        }
    }
    Ok(())
}

async fn cmd_secrets_set(client: &OxClient, name: &str, value: Option<String>) -> Result<()> {
    let value = match value {
        Some(v) => v,
        None => {
            // Read from stdin
            use std::io::Read;
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf)?;
            buf.trim_end().to_string()
        }
    };
    client.set_secret(name, &value).await?;
    println!("Set {name}");
    Ok(())
}

async fn cmd_secrets_delete(client: &OxClient, name: &str) -> Result<()> {
    client.delete_secret(name).await?;
    println!("Deleted {name}");
    Ok(())
}

// ── Workflows ───────────────────────────────────────────────────────

async fn cmd_workflows(client: &OxClient, json: bool) -> Result<()> {
    let workflows = client.list_workflows().await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&workflows
            .iter()
            .map(|w| serde_json::json!({"name": w.name, "steps": w.steps}))
            .collect::<Vec<_>>())?);
    } else {
        println!("{:<20} {:<8} STEPS", "NAME", "COUNT");
        for w in &workflows {
            println!("{:<20} {:<8} {}", w.name, w.steps.len(), w.steps.join(", "));
        }
    }
    Ok(())
}

// ── Events ──────────────────────────────────────────────────────────

async fn cmd_events(
    server_url: &str,
    json: bool,
    since: Option<u64>,
    type_filter: Option<String>,
) -> Result<()> {
    use futures_util::StreamExt;
    use reqwest_eventsource::{Event as SseEvent, EventSource};

    let url = match since {
        Some(seq) => format!("{server_url}/api/events/stream?last_event_id={seq}"),
        None => format!("{server_url}/api/events/stream"),
    };
    let mut es = EventSource::get(&url);

    loop {
        match es.next().await {
            Some(Ok(SseEvent::Message(msg))) => {
                // Apply type filter
                if let Some(ref filter) = type_filter
                    && !msg.event.starts_with(filter) {
                        continue;
                    }

                if json {
                    println!("{}", msg.data);
                } else {
                    // Parse for summary
                    let seq = &msg.id;
                    let event_type = &msg.event;
                    let summary = event_summary(&msg.data);
                    println!("{:<6} {:<24} {}", seq, event_type, summary);
                }
            }
            Some(Ok(SseEvent::Open)) => {}
            Some(Err(reqwest_eventsource::Error::StreamEnded)) => break,
            Some(Err(e)) => {
                eprintln!("SSE error: {e}");
                break;
            }
            None => break,
        }
    }
    Ok(())
}

fn event_summary(data: &str) -> String {
    let v: serde_json::Value = match serde_json::from_str(data) {
        Ok(v) => v,
        Err(_) => return String::new(),
    };
    // Non-ox events render as "source subject_id" — the kind already
    // shows in the TYPE column so don't repeat it.
    let source = v.get("source").and_then(|x| x.as_str()).unwrap_or("");
    if !source.is_empty() && source != ox_core::events::SOURCE_OX {
        let subj = v.get("subject_id").and_then(|x| x.as_str()).unwrap_or("");
        return if subj.is_empty() {
            source.to_string()
        } else {
            format!("{source} {subj}")
        };
    }

    let d = v.get("data").unwrap_or(&v);

    // Build a brief summary from common fields
    let mut parts = vec![];
    if let Some(eid) = d.get("execution_id").and_then(|v| v.as_str()) {
        parts.push(eid.to_string());
    }
    if let Some(rid) = d.get("runner_id").and_then(|v| v.as_str()) {
        parts.push(rid.to_string());
    }
    if let Some(s) = d.get("step").and_then(|v| v.as_str()) {
        parts.push(s.to_string());
    }
    if let Some(o) = d.get("output").and_then(|v| v.as_str()) {
        parts.push(format!("output={o}"));
    }
    if let Some(n) = d.get("name").and_then(|v| v.as_str()) {
        parts.push(n.to_string());
    }
    if let Some(e) = d.get("error").and_then(|v| v.as_str()) {
        parts.push(format!("error={e}"));
    }
    parts.join(" ")
}

// ── Attach ──────────────────────────────────────────────────────────

async fn cmd_attach(server_url: &str, execution_id: &str, step: &str) -> Result<()> {
    use futures_util::{SinkExt, StreamExt};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio_tungstenite::tungstenite::Message;

    // Check execution state before connecting
    let client = OxClient::new(server_url);
    let exec = client
        .get_execution(execution_id)
        .await
        .map_err(|_| anyhow::anyhow!("execution {execution_id} not found"))?;
    if exec.status != "running" {
        anyhow::bail!("execution {execution_id} is {}, not running", exec.status);
    }
    if exec.current_step.as_deref() != Some(step) {
        anyhow::bail!(
            "current step is '{}', not '{step}'",
            exec.current_step.as_deref().unwrap_or("none")
        );
    }

    let ws_url = server_url
        .replace("http://", "ws://")
        .replace("https://", "wss://");
    let url = format!(
        "{}/api/executions/{}/steps/{}/pty",
        ws_url, execution_id, step
    );

    // Connect websocket — retry a few times since the runner may not have
    // established its relay yet.
    let mut ws = None;
    for attempt in 0..10u32 {
        match tokio_tungstenite::connect_async(&url).await {
            Ok((stream, _)) => {
                ws = Some(stream);
                break;
            }
            Err(_) if attempt < 9 => {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
            Err(e) => {
                anyhow::bail!("failed to connect to PTY relay: {e}");
            }
        }
    }
    let mut ws = ws.unwrap();

    // Enter raw mode
    let orig_termios = unsafe {
        let mut termios: libc::termios = std::mem::zeroed();
        libc::tcgetattr(0, &mut termios);
        let orig = termios;
        libc::cfmakeraw(&mut termios);
        libc::tcsetattr(0, libc::TCSANOW, &termios);
        orig
    };

    let restore = move || unsafe {
        libc::tcsetattr(0, libc::TCSANOW, &orig_termios);
    };

    // Send a newline to nudge the shell into printing its prompt
    let _ = ws.send(Message::Binary(b"\n".to_vec().into())).await;

    let mut stdout = tokio::io::stdout();

    // Split ws so we can read and write concurrently
    let (ws_tx, mut ws_rx) = ws.split();

    // Periodic check: is the step still running?
    let poll_client = OxClient::new(server_url);
    let poll_exec_id = execution_id.to_string();
    let poll_step = step.to_string();
    let step_alive = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let step_alive_writer = step_alive.clone();

    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
        loop {
            interval.tick().await;
            if let Ok(exec) = poll_client.get_execution(&poll_exec_id).await
                && (exec.status != "running"
                    || exec.current_step.as_deref() != Some(&poll_step))
            {
                step_alive_writer.store(false, std::sync::atomic::Ordering::Relaxed);
                break;
            }
        }
    });

    // stdin → ws (spawned task)
    let stdin_task = tokio::spawn(async move {
        let mut ws_tx = ws_tx;  // move into task
        let mut stdin = tokio::io::stdin();
        let mut buf = [0u8; 1024];
        loop {
            let n = match stdin.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            if ws_tx.send(Message::Binary(buf[..n].to_vec().into())).await.is_err() {
                break;
            }
        }
    });

    // ws → stdout (main loop)
    loop {
        // Check if step is still alive
        if !step_alive.load(std::sync::atomic::Ordering::Relaxed) {
            break;
        }

        match tokio::time::timeout(std::time::Duration::from_secs(1), ws_rx.next()).await {
            Ok(Some(Ok(Message::Binary(data)))) => {
                let _ = stdout.write_all(&data).await;
                let _ = stdout.flush().await;
            }
            Ok(Some(Ok(Message::Close(_)))) | Ok(None) => break,
            Ok(Some(Err(_))) => break,
            Ok(Some(Ok(Message::Text(t)))) if t.as_str() == "__ox_pty_eof__" => break,
            Err(_) => continue, // timeout — check step_alive and retry
            _ => {}
        }
    }

    stdin_task.abort();
    restore();
    eprintln!("\r\n[ox-ctl: session ended]");
    std::process::exit(0);
}

// ── Logs ────────────────────────────────────────────────────────────

async fn cmd_logs(
    server_url: &str,
    execution_id: &str,
    step: &str,
    attempt: Option<u32>,
    lines: Option<usize>,
    follow: bool,
    pretty: bool,
) -> Result<()> {
    let client = reqwest::Client::new();
    let base_url = format!("{server_url}/api/executions/{execution_id}/steps/{step}/log");

    let mut params = vec![];
    if let Some(a) = attempt {
        params.push(format!("attempt={a}"));
    }
    if let Some(n) = lines {
        params.push(format!("lines={n}"));
    }

    let url = if params.is_empty() {
        base_url.clone()
    } else {
        format!("{}?{}", base_url, params.join("&"))
    };

    let resp = client.get(&url).send().await?;

    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        if !follow {
            eprintln!("No logs found for {execution_id} step {step}");
            return Ok(());
        }
        // In follow mode, wait for logs to appear
    } else {
        let text = resp.error_for_status()?.text().await?;
        if pretty {
            pretty_print_log(&text);
        } else {
            print!("{text}");
        }

        if !follow {
            return Ok(());
        }
    }

    // Follow mode: poll for new content
    let mut known_len: usize = 0;
    // Compute initial length from what we already printed
    {
        let mut check_params = vec![];
        if let Some(a) = attempt {
            check_params.push(format!("attempt={a}"));
        }
        let check_url = if check_params.is_empty() {
            base_url.clone()
        } else {
            format!("{}?{}", base_url, check_params.join("&"))
        };
        if let Ok(resp) = client.get(&check_url).send().await
            && let Ok(text) = resp.text().await {
                known_len = text.len();
            }
    }

    let mut poll_params = vec![];
    if let Some(a) = attempt {
        poll_params.push(format!("attempt={a}"));
    }

    loop {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        let poll_url = if poll_params.is_empty() {
            base_url.clone()
        } else {
            format!("{}?{}", base_url, poll_params.join("&"))
        };

        let resp = match client.get(&poll_url).send().await {
            Ok(r) => r,
            Err(_) => continue,
        };

        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            continue;
        }

        if let Ok(text) = resp.text().await
            && text.len() > known_len {
                let new_data = &text[known_len..];
                if pretty {
                    pretty_print_log(new_data);
                } else {
                    print!("{new_data}");
                }
                known_len = text.len();
            }
    }
}

const DIM: &str = "\x1b[2m";
const BOLD: &str = "\x1b[1m";
const CYAN: &str = "\x1b[36m";
const RED: &str = "\x1b[31m";
const RESET: &str = "\x1b[0m";

/// Pretty-print a stream-json log (Claude or Codex) to stdout.
fn pretty_print_log(text: &str) {
    let mut out = String::new();
    for line in text.lines() {
        render_log_line(&mut out, line);
    }
    print!("{out}");
}

/// Render a single ndjson log line, dispatching to the Claude or Codex
/// renderer based on the event's top-level `type` field.
fn render_log_line(out: &mut String, line: &str) {
    use std::fmt::Write as _;

    let line = line.trim();
    if line.is_empty() {
        return;
    }
    let v: serde_json::Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => {
            let _ = writeln!(out, "{line}");
            return;
        }
    };
    let ty = v["type"].as_str().unwrap_or("");
    if ty.starts_with("thread.") || ty.starts_with("turn.") || ty.starts_with("item.") {
        render_codex_event(out, &v);
    } else {
        render_claude_event(out, &v);
    }
}

/// Codex wraps shell tool calls as `/bin/bash -lc "…"`. Strip that wrapper
/// so the rendered `$` line shows the inner command directly. If the input
/// doesn't match the wrapper shape, return it unchanged.
fn strip_bash_wrapper(cmd: &str) -> String {
    let prefix = "/bin/bash -lc \"";
    if let Some(rest) = cmd.strip_prefix(prefix)
        && let Some(inner) = rest.strip_suffix('"')
    {
        return inner.replace("\\\"", "\"");
    }
    cmd.to_string()
}

fn render_codex_event(out: &mut String, v: &serde_json::Value) {
    use std::fmt::Write as _;

    match v["type"].as_str().unwrap_or("") {
        "thread.started" => {
            let thread = v["thread_id"].as_str().unwrap_or("?");
            let _ = writeln!(out, "{DIM}── session: thread={thread} ──{RESET}");
        }
        "turn.started" => {}
        "turn.completed" => {
            let tokens_in = v["usage"]["input_tokens"].as_u64().unwrap_or(0);
            let cached = v["usage"]["cached_input_tokens"].as_u64().unwrap_or(0);
            let tokens_out = v["usage"]["output_tokens"].as_u64().unwrap_or(0);
            let _ = writeln!(
                out,
                "{DIM}── done: {tokens_in}in ({cached} cached) / {tokens_out}out ──{RESET}"
            );
        }
        "item.completed" => {
            let item = &v["item"];
            match item["type"].as_str().unwrap_or("") {
                "agent_message" => {
                    let text = item["text"].as_str().unwrap_or("");
                    if !text.is_empty() {
                        let _ = writeln!(out, "{BOLD}{text}{RESET}");
                    }
                }
                "command_execution" => {
                    let exit = item["exit_code"].as_i64().unwrap_or(0);
                    let agg = item["aggregated_output"].as_str().unwrap_or("");
                    if exit != 0 {
                        let short = if agg.len() > 200 {
                            format!("{}...", &agg[..200])
                        } else {
                            agg.to_string()
                        };
                        let _ = writeln!(out, "{RED}  error (exit {exit}): {short}{RESET}");
                    } else if !agg.is_empty() {
                        let first = agg.lines().next().unwrap_or("");
                        let first = if first.len() > 120 {
                            format!("{}...", &first[..120])
                        } else {
                            first.to_string()
                        };
                        let _ = writeln!(out, "{DIM}  → {first}{RESET}");
                    }
                }
                _ => {}
            }
        }
        "item.started" => {
            let item = &v["item"];
            match item["type"].as_str().unwrap_or("") {
                "command_execution" => {
                    let cmd = item["command"].as_str().unwrap_or("");
                    let shown = strip_bash_wrapper(cmd);
                    let _ = writeln!(out, "{CYAN}$ {shown}{RESET}");
                }
                "file_change" => {
                    if let Some(changes) = item["changes"].as_array() {
                        for change in changes {
                            let kind = change["kind"].as_str().unwrap_or("?");
                            let path = change["path"].as_str().unwrap_or("?");
                            let _ = writeln!(out, "{CYAN}  {kind} {path}{RESET}");
                        }
                    }
                }
                _ => {}
            }
        }
        _ => {}
    }
}

fn render_claude_event(out: &mut String, v: &serde_json::Value) {
    use std::fmt::Write as _;

    match v["type"].as_str().unwrap_or("") {
        "system" => {
            if v["subtype"].as_str() == Some("init") {
                let model = v["model"].as_str().unwrap_or("?");
                let cwd = v["cwd"].as_str().unwrap_or("?");
                let _ = writeln!(out, "{DIM}── session: model={model} cwd={cwd} ──{RESET}");
            }
        }
        "assistant" => {
            let content = v["message"]["content"].as_array();
            if let Some(blocks) = content {
                for block in blocks {
                    match block["type"].as_str().unwrap_or("") {
                        "text" => {
                            let text = block["text"].as_str().unwrap_or("");
                            if !text.is_empty() {
                                let _ = writeln!(out, "{BOLD}{text}{RESET}");
                            }
                        }
                        "tool_use" => {
                            let name = block["name"].as_str().unwrap_or("?");
                            let input = &block["input"];
                            match name {
                                "Bash" => {
                                    let cmd = input["command"].as_str().unwrap_or("");
                                    let desc = input["description"].as_str().unwrap_or("");
                                    if !desc.is_empty() {
                                        let _ = writeln!(out, "{CYAN}$ {cmd}{RESET}  {DIM}# {desc}{RESET}");
                                    } else {
                                        let _ = writeln!(out, "{CYAN}$ {cmd}{RESET}");
                                    }
                                }
                                "Read" => {
                                    let path = input["file_path"].as_str().unwrap_or("?");
                                    let _ = writeln!(out, "{CYAN}  read {path}{RESET}");
                                }
                                "Write" => {
                                    let path = input["file_path"].as_str().unwrap_or("?");
                                    let _ = writeln!(out, "{CYAN}  write {path}{RESET}");
                                }
                                "Edit" => {
                                    let path = input["file_path"].as_str().unwrap_or("?");
                                    let _ = writeln!(out, "{CYAN}  edit {path}{RESET}");
                                }
                                "Glob" => {
                                    let pattern = input["pattern"].as_str().unwrap_or("?");
                                    let _ = writeln!(out, "{CYAN}  glob {pattern}{RESET}");
                                }
                                "Grep" => {
                                    let pattern = input["pattern"].as_str().unwrap_or("?");
                                    let _ = writeln!(out, "{CYAN}  grep {pattern}{RESET}");
                                }
                                _ => {
                                    let short = serde_json::to_string(input).unwrap_or_default();
                                    let short = if short.len() > 80 {
                                        format!("{}...", &short[..80])
                                    } else {
                                        short
                                    };
                                    let _ = writeln!(out, "{CYAN}  {name}({short}){RESET}");
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        "user" => {
            let content = v["message"]["content"].as_array();
            if let Some(blocks) = content {
                for block in blocks {
                    if block["type"].as_str() == Some("tool_result") {
                        let is_error = block["is_error"].as_bool().unwrap_or(false);
                        let content_str = block["content"].as_str().unwrap_or("");
                        if is_error {
                            let short = if content_str.len() > 200 {
                                format!("{}...", &content_str[..200])
                            } else {
                                content_str.to_string()
                            };
                            let _ = writeln!(out, "{RED}  error: {short}{RESET}");
                        } else if !content_str.is_empty() {
                            let first_line = content_str.lines().next().unwrap_or("");
                            let first_line = if first_line.len() > 120 {
                                format!("{}...", &first_line[..120])
                            } else {
                                first_line.to_string()
                            };
                            let _ = writeln!(out, "{DIM}  → {first_line}{RESET}");
                        }
                    }
                }
            }
        }
        "result" => {
            let cost = v["cost_usd"].as_f64();
            let duration = v["duration_ms"].as_u64();
            let tokens_in = v["usage"]["input_tokens"].as_u64().unwrap_or(0);
            let tokens_out = v["usage"]["output_tokens"].as_u64().unwrap_or(0);
            if let (Some(cost), Some(dur)) = (cost, duration) {
                let _ = writeln!(
                    out,
                    "{DIM}── done: {:.1}s, {tokens_in}in/{tokens_out}out, ${cost:.4} ──{RESET}",
                    dur as f64 / 1000.0
                );
            }
        }
        _ => {}
    }
}

// ── Trigger ─────────────────────────────────────────────────────────

async fn cmd_trigger(client: &OxClient, node_id: &str, force: bool) -> Result<()> {
    let resp = client.trigger(node_id, force).await?;
    println!("{}", serde_json::to_string_pretty(&resp)?);
    Ok(())
}

// ── Config ──────────────────────────────────────────────────────────

async fn cmd_reload(client: &OxClient, json: bool) -> Result<()> {
    let resp = client.reload_config().await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&resp)?);
    } else if resp.get("status").and_then(|s| s.as_str()) == Some("ok") {
        println!("Config reloaded:");
        if let Some(w) = resp.get("workflows") { println!("  workflows: {w}"); }
        if let Some(r) = resp.get("runtimes") { println!("  runtimes:  {r}"); }
        if let Some(p) = resp.get("personas") { println!("  personas:  {p}"); }
        if let Some(t) = resp.get("triggers") { println!("  triggers:  {t}"); }
    } else {
        eprintln!("Reload failed:");
        if let Some(errors) = resp.get("errors").and_then(|e| e.as_array()) {
            for err in errors {
                eprintln!("  {}", err.as_str().unwrap_or(&err.to_string()));
            }
        }
        std::process::exit(1);
    }
    Ok(())
}

async fn cmd_config_check(client: &OxClient, json: bool) -> Result<()> {
    let resp = client.check_config().await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&resp)?);
    } else if resp.get("valid").and_then(|v| v.as_bool()) == Some(true) {
        println!("Config valid.");
        if let Some(changes) = resp.get("changes") {
            for (category, diff) in changes.as_object().into_iter().flatten() {
                let added = diff.get("added").and_then(|a| a.as_array()).map(|a| a.len()).unwrap_or(0);
                let removed = diff.get("removed").and_then(|a| a.as_array()).map(|a| a.len()).unwrap_or(0);
                if added > 0 || removed > 0 {
                    println!("  {category}: +{added} -{removed}");
                }
            }
        }
    } else {
        eprintln!("Config invalid:");
        if let Some(errors) = resp.get("errors").and_then(|e| e.as_array()) {
            for err in errors {
                eprintln!("  {}", err.as_str().unwrap_or(&err.to_string()));
            }
        }
        std::process::exit(1);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use ox_core::events::ExecutionOrigin;
    use ox_core::types::Seq;

    #[test]
    fn event_summary_source_event() {
        // Canonical envelope: source/kind/subject_id live at the top level.
        let data = r#"{"seq":1,"ts":"2026-01-01T00:00:00Z","source":"cx","kind":"node.ready","subject_id":"aJuO","data":{"tags":["workflow:code-task"]}}"#;
        assert_eq!(event_summary(data), "cx aJuO");
    }

    #[test]
    fn event_summary_step_done() {
        let data = r#"{"seq":1,"ts":"2026-01-01T00:00:00Z","source":"ox","kind":"step.done","subject_id":"e-1","data":{"execution_id":"e-1","step":"propose","output":"proposed"}}"#;
        assert_eq!(event_summary(data), "e-1 propose output=proposed");
    }

    #[test]
    fn format_origin_event_cx_node() {
        let o = ExecutionOrigin::Event {
            source: "cx".into(),
            kind: "node.ready".into(),
            subject_id: "aJuO".into(),
            seq: Seq(42),
        };
        assert_eq!(format_origin(&o), "cx:aJuO");
    }

    #[test]
    fn format_origin_manual_anonymous() {
        let o = ExecutionOrigin::Manual { user: None };
        assert_eq!(format_origin(&o), "manual");
    }

    #[test]
    fn format_origin_manual_with_user() {
        let o = ExecutionOrigin::Manual {
            user: Some("alice".into()),
        };
        assert_eq!(format_origin(&o), "manual:alice");
    }

    #[test]
    fn format_origin_truncates_long_values_with_ellipsis() {
        let o = ExecutionOrigin::Event {
            source: "cx".into(),
            kind: "node.ready".into(),
            subject_id: "thisIsAReallyLongNodeIdentifierForcingTruncation".into(),
            seq: Seq(1),
        };
        let s = format_origin(&o);
        assert!(s.chars().count() <= 24, "got {:?}", s);
        assert!(s.ends_with('…'), "got {:?}", s);
        assert!(s.starts_with("cx:"), "got {:?}", s);
    }

    #[test]
    fn clap_rejects_task_flag_on_exec_list() {
        // --task was removed in slice D. clap must surface it as unknown.
        let result = Cli::try_parse_from(["ox-ctl", "exec", "list", "--task", "aJuO"]);
        assert!(result.is_err(), "--task should be rejected");
    }

    // ── pretty_print_log: Claude regression ──────────────────────────

    fn render(line: &str) -> String {
        let mut out = String::new();
        render_log_line(&mut out, line);
        out
    }

    #[test]
    fn claude_assistant_text_is_bold() {
        let out = render(
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hello world"}]}}"#,
        );
        assert!(out.contains("hello world"), "got {out:?}");
        assert!(out.contains(BOLD), "missing bold: {out:?}");
    }

    #[test]
    fn claude_system_init_shows_session_header() {
        let out = render(
            r#"{"type":"system","subtype":"init","model":"sonnet-4-6","cwd":"/tmp/x"}"#,
        );
        assert!(out.contains("session:"));
        assert!(out.contains("model=sonnet-4-6"));
        assert!(out.contains("cwd=/tmp/x"));
    }

    #[test]
    fn claude_bash_tool_use_renders_dollar_prompt() {
        let out = render(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"ls -la","description":"list"}}]}}"#,
        );
        assert!(out.contains("$ ls -la"));
        assert!(out.contains("# list"));
    }

    #[test]
    fn claude_tool_result_error_is_red() {
        let out = render(
            r#"{"type":"user","message":{"content":[{"type":"tool_result","is_error":true,"content":"boom"}]}}"#,
        );
        assert!(out.contains("error: boom"));
        assert!(out.contains(RED));
    }

    #[test]
    fn claude_result_shows_done_footer() {
        let out = render(
            r#"{"type":"result","cost_usd":0.12,"duration_ms":3400,"usage":{"input_tokens":100,"output_tokens":42}}"#,
        );
        assert!(out.contains("done:"));
        assert!(out.contains("100in/42out"));
        assert!(out.contains("$0.1200"));
    }

    #[test]
    fn invalid_json_falls_through_to_raw() {
        let out = render("not json at all");
        assert_eq!(out, "not json at all\n");
    }

    #[test]
    fn empty_line_renders_nothing() {
        let out = render("   ");
        assert_eq!(out, "");
    }

    // ── pretty_print_log: Codex ───────────────────────────────────────

    #[test]
    fn codex_thread_started_shows_session_header() {
        let out = render(
            r#"{"type":"thread.started","thread_id":"019d8de2-fb51-7bb2-a40d-b02f4475724f"}"#,
        );
        assert!(out.contains("session:"), "got {out:?}");
        assert!(out.contains("thread="), "got {out:?}");
        // Full id should appear — short-form was considered but kept full for grep.
        assert!(out.contains("019d8de2-fb51-7bb2-a40d-b02f4475724f"));
        assert!(out.contains(DIM));
    }

    #[test]
    fn codex_turn_started_is_silent() {
        let out = render(r#"{"type":"turn.started"}"#);
        assert_eq!(out, "");
    }

    #[test]
    fn codex_agent_message_is_bold() {
        let out = render(
            r#"{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"I'll read the file first."}}"#,
        );
        assert!(out.contains("I'll read the file first."));
        assert!(out.contains(BOLD));
    }

    #[test]
    fn codex_agent_message_on_item_started_is_silent() {
        // agent_message is only rendered on completion to avoid double output.
        let out = render(
            r#"{"type":"item.started","item":{"id":"item_0","type":"agent_message","text":"partial"}}"#,
        );
        assert_eq!(out, "");
    }

    #[test]
    fn codex_command_execution_started_prints_dollar_prompt() {
        let out = render(
            r#"{"type":"item.started","item":{"id":"item_1","type":"command_execution","command":"/bin/bash -lc \"sed -n '1,5p' /tmp/x\"","aggregated_output":"","exit_code":null,"status":"in_progress"}}"#,
        );
        // Bash wrapper stripped — inner command shown.
        assert!(out.contains("$ sed -n '1,5p' /tmp/x"), "got {out:?}");
        assert!(!out.contains("/bin/bash -lc"), "wrapper leaked: {out:?}");
        assert!(out.contains(CYAN));
    }

    #[test]
    fn codex_command_execution_without_bash_wrapper_prints_raw() {
        let out = render(
            r#"{"type":"item.started","item":{"id":"i","type":"command_execution","command":"ls /tmp","aggregated_output":"","exit_code":null,"status":"in_progress"}}"#,
        );
        assert!(out.contains("$ ls /tmp"));
    }

    #[test]
    fn codex_command_execution_completed_success_shows_dim_output_line() {
        let out = render(
            r#"{"type":"item.completed","item":{"id":"i","type":"command_execution","command":"ls","aggregated_output":"file1.txt\nfile2.txt\n","exit_code":0,"status":"completed"}}"#,
        );
        assert!(out.contains("→ file1.txt"), "got {out:?}");
        assert!(out.contains(DIM));
        assert!(!out.contains("file2.txt"), "only first line should render: {out:?}");
    }

    #[test]
    fn codex_command_execution_completed_error_shows_red() {
        let out = render(
            r#"{"type":"item.completed","item":{"id":"i","type":"command_execution","command":"false","aggregated_output":"permission denied","exit_code":1,"status":"completed"}}"#,
        );
        assert!(out.contains("error"));
        assert!(out.contains("permission denied"));
        assert!(out.contains(RED));
    }

    #[test]
    fn codex_file_change_started_lists_each_path() {
        let out = render(
            r#"{"type":"item.started","item":{"id":"i","type":"file_change","changes":[{"path":"/tmp/a.md","kind":"add"},{"path":"/tmp/b.md","kind":"update"}],"status":"in_progress"}}"#,
        );
        assert!(out.contains("add /tmp/a.md"), "got {out:?}");
        assert!(out.contains("update /tmp/b.md"), "got {out:?}");
        assert!(out.contains(CYAN));
    }

    #[test]
    fn codex_file_change_completed_is_silent() {
        // Already announced on start — completion is noise.
        let out = render(
            r#"{"type":"item.completed","item":{"id":"i","type":"file_change","changes":[{"path":"/tmp/a.md","kind":"add"}],"status":"completed"}}"#,
        );
        assert_eq!(out, "");
    }

    #[test]
    fn codex_turn_completed_shows_done_footer() {
        let out = render(
            r#"{"type":"turn.completed","usage":{"input_tokens":289532,"cached_input_tokens":268672,"output_tokens":2213}}"#,
        );
        assert!(out.contains("done:"), "got {out:?}");
        assert!(out.contains("289532in"));
        assert!(out.contains("2213out"));
        assert!(out.contains("268672 cached"));
        assert!(out.contains(DIM));
    }

    #[test]
    fn clap_accepts_status_and_workflow_filters() {
        let result = Cli::try_parse_from([
            "ox-ctl",
            "exec",
            "list",
            "--status",
            "running",
            "--workflow",
            "consultation",
        ]);
        assert!(result.is_ok(), "--status/--workflow should parse cleanly");
    }

    // ── format_watchers_section (slice 4) ─────────────────────────

    fn row(
        source: &str,
        cursor: Option<&str>,
        updated_at: &str,
        last_error: Option<&str>,
    ) -> WatcherRow {
        WatcherRow {
            source: source.into(),
            cursor: cursor.map(String::from),
            updated_at: updated_at.into(),
            last_error: last_error.map(String::from),
        }
    }

    fn runner_row(
        id: &str,
        status: &str,
        workflow: Option<&str>,
        exec_id: Option<&str>,
        step: Option<&str>,
        attempt: Option<u32>,
    ) -> RunnerRow {
        RunnerRow {
            id: id.into(),
            status: status.into(),
            workflow: workflow.map(String::from),
            exec_id: exec_id.map(String::from),
            step: step.map(String::from),
            attempt,
        }
    }

    #[test]
    fn format_runners_section_empty_returns_empty_string() {
        assert_eq!(format_runners_section(&[]), "");
    }

    #[test]
    fn format_runners_section_renders_header_and_busy_row() {
        let out = format_runners_section(&[runner_row(
            "run-4a2f",
            "executing",
            Some("deploy-api"),
            Some("aJuO-e1"),
            Some("propose"),
            Some(2),
        )]);
        assert!(out.contains("ID"), "header missing ID: {out}");
        assert!(out.contains("STATUS"), "header missing STATUS: {out}");
        assert!(out.contains("WORKFLOW"), "header missing WORKFLOW: {out}");
        assert!(out.contains("STEP"), "header missing STEP: {out}");
        assert!(out.contains("run-4a2f"));
        assert!(out.contains("executing"));
        assert!(out.contains("deploy-api"));
        assert!(out.contains("aJuO-e1"));
        assert!(out.contains("propose"));
        // Attempt number surfaces somewhere (e.g. "propose#2" or "propose/2").
        assert!(out.contains('2'), "attempt number missing: {out}");
    }

    #[test]
    fn format_runners_section_renders_idle_as_dashes() {
        let out = format_runners_section(&[runner_row(
            "run-91bc",
            "idle",
            None,
            None,
            None,
            None,
        )]);
        assert!(out.contains("run-91bc"));
        assert!(out.contains("idle"));
        // No raw "None" or "null" in the display.
        assert!(
            !out.contains("None") && !out.contains("null"),
            "raw null should not leak: {out}"
        );
        // A dash placeholder appears for the missing workflow/step fields.
        assert!(out.contains('-'), "expected dash placeholder, got: {out}");
    }

    #[test]
    fn format_runners_section_renders_multiple_rows() {
        let out = format_runners_section(&[
            runner_row(
                "run-4a2f",
                "executing",
                Some("deploy-api"),
                Some("aJuO-e1"),
                Some("propose"),
                Some(2),
            ),
            runner_row("run-91bc", "idle", None, None, None, None),
        ]);
        assert!(out.contains("run-4a2f"));
        assert!(out.contains("run-91bc"));
        let lines: Vec<_> = out.lines().collect();
        assert!(
            lines.len() >= 3,
            "expected header + 2 data rows, got {lines:?}"
        );
    }

    #[test]
    fn parse_step_attempt_canonical() {
        assert_eq!(
            parse_step_attempt("aJuO-e1/propose/2"),
            Some(("aJuO-e1".to_string(), "propose".to_string(), 2))
        );
    }

    #[test]
    fn parse_step_attempt_empty_returns_none() {
        assert_eq!(parse_step_attempt(""), None);
    }

    #[test]
    fn parse_step_attempt_missing_parts_returns_none() {
        assert_eq!(parse_step_attempt("aJuO-e1"), None);
        assert_eq!(parse_step_attempt("aJuO-e1/propose"), None);
    }

    #[test]
    fn parse_step_attempt_non_numeric_attempt_returns_none() {
        assert_eq!(parse_step_attempt("aJuO-e1/propose/abc"), None);
    }

    #[test]
    fn format_watchers_section_empty_returns_empty_string() {
        assert_eq!(format_watchers_section(&[]), "");
    }

    #[test]
    fn format_watchers_section_renders_header_and_alive_row() {
        let out = format_watchers_section(&[row(
            "cx",
            Some("d59b010abc12def3"),
            "2026-04-15T12:00:03Z",
            None,
        )]);
        assert!(out.contains("SOURCE"), "header row missing: {out}");
        assert!(out.contains("LAST INGEST"));
        assert!(out.contains("CURSOR"));
        assert!(out.contains("STATUS"));
        assert!(out.contains("cx"));
        assert!(out.contains("2026-04-15T12:00:03Z"));
        assert!(out.contains("alive"));
        // Full cursor truncated for display — leading prefix still present.
        assert!(out.contains("d59b010abc12"), "cursor prefix missing in: {out}");
    }

    #[test]
    fn format_watchers_section_shows_last_error_in_status_column() {
        let out = format_watchers_section(&[row(
            "cx",
            Some("sha-abc"),
            "2026-04-15T12:00:03Z",
            Some("cas:expected None got \"sha-real\""),
        )]);
        assert!(
            out.contains("cas:expected"),
            "last_error should appear in status column: {out}"
        );
        assert!(
            !out.contains(" alive "),
            "alive must not be shown when last_error is set: {out}"
        );
    }

    #[test]
    fn format_watchers_section_renders_null_cursor_as_placeholder() {
        let out = format_watchers_section(&[row(
            "cx",
            None,
            "2026-04-15T12:00:03Z",
            None,
        )]);
        // Missing cursor: show "-" or similar placeholder, not "null".
        assert!(
            !out.contains("null"),
            "raw null should not leak to display: {out}"
        );
        assert!(
            out.contains(" - ") || out.contains(" -\n") || out.contains("  -"),
            "expected dash placeholder, got: {out}"
        );
    }

    #[test]
    fn format_watchers_section_renders_multiple_rows() {
        let out = format_watchers_section(&[
            row("cx", Some("aaaaaaaabbbbcccc"), "2026-04-15T11:00:00Z", None),
            row(
                "linear",
                Some("2026-04-15T12:00:00Z"),
                "2026-04-15T12:00:03Z",
                None,
            ),
        ]);
        assert!(out.contains("cx"));
        assert!(out.contains("linear"));
        // Both have their own line.
        let lines: Vec<_> = out.lines().collect();
        assert!(
            lines.len() >= 3,
            "expected header + 2 data rows, got {lines:?}"
        );
    }
}
