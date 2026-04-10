mod output;

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
}

#[derive(Subcommand)]
enum ExecCommands {
    /// List executions.
    List {
        #[arg(long)]
        status: Option<String>,
        #[arg(long)]
        workflow: Option<String>,
        #[arg(long)]
        task: Option<String>,
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
        /// Pretty-print Claude Code stream-json logs.
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

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let client = OxClient::new(&cli.server);
    let json = cli.json;

    match cli.command {
        Commands::Status => cmd_status(&client, json).await,
        Commands::Exec { command } => match command {
            ExecCommands::List { status, workflow, task } => {
                cmd_exec_list(&client, json, status, workflow, task).await
            }
            ExecCommands::Show { id } => cmd_exec_show(&client, json, &id).await,
            ExecCommands::Cancel { id } => cmd_exec_cancel(&client, &id).await,
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
    }
}

// ── Status ──────────────────────────────────────────────────────────

async fn cmd_status(client: &OxClient, json: bool) -> Result<()> {
    let s = client.status().await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&serde_json::json!({
            "status": s.status,
            "pool_size": s.pool_size,
            "pool_executing": s.pool_executing,
            "pool_idle": s.pool_idle,
            "executions_running": s.executions_running,
            "workflows_loaded": s.workflows_loaded,
            "event_seq": s.event_seq,
        }))?);
    } else {
        println!("ox-server   {}   seq {}", s.status, s.event_seq);
        println!(
            "pool        {} runners ({} executing, {} idle)",
            s.pool_size, s.pool_executing, s.pool_idle
        );
        println!("executions  {} running", s.executions_running);
        println!("workflows   {} loaded", s.workflows_loaded);
    }
    Ok(())
}

// ── Executions ──────────────────────────────────────────────────────

async fn cmd_exec_list(
    client: &OxClient,
    json: bool,
    _status: Option<String>,
    _workflow: Option<String>,
    _task: Option<String>,
) -> Result<()> {
    let execs = client.list_executions().await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&execs)?);
    } else {
        println!(
            "{:<14} {:<8} {:<16} {:<14} {:<12}",
            "ID", "TASK", "WORKFLOW", "STEP", "STATUS"
        );
        for e in &execs {
            let status = e.get("status").and_then(|v| v.as_str()).unwrap_or("-");
            let step = e.get("current_step").and_then(|v| v.as_str()).unwrap_or("-");
            // If running but no step assigned yet, show as pending
            let display_status = if status == "running" && step == "-" {
                "pending"
            } else {
                status
            };
            println!(
                "{:<14} {:<8} {:<16} {:<14} {:<12}",
                e.get("id").and_then(|v| v.as_str()).unwrap_or("-"),
                e.get("task_id").and_then(|v| v.as_str()).unwrap_or("-"),
                e.get("workflow").and_then(|v| v.as_str()).unwrap_or("-"),
                step,
                display_status,
            );
        }
    }
    Ok(())
}

async fn cmd_exec_show(client: &OxClient, json: bool, id: &str) -> Result<()> {
    let exec = client.get_execution(id).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&serde_json::json!({
            "id": exec.id,
            "task_id": exec.task_id,
            "workflow": exec.workflow,
            "status": exec.status,
            "current_step": exec.current_step,
            "attempts": exec.attempts,
        }))?);
    } else {
        println!("Execution: {}", exec.id);
        println!("Task:      {}", exec.task_id);
        println!("Workflow:  {}", exec.workflow);
        println!("Status:    {}", exec.status);
        println!();
        println!(
            "{:<4} {:<14} {:<8} {:<10} {:<10} {:<12} {:<16}",
            "#", "STEP", "ATTEMPT", "STATUS", "RUNNER", "OUTPUT", "TRANSITION"
        );
        for (i, a) in exec.attempts.iter().enumerate() {
            println!(
                "{:<4} {:<14} {:<8} {:<10} {:<10} {:<12} {:<16}",
                i + 1,
                a.step,
                a.attempt,
                a.status,
                a.runner_id.as_deref().unwrap_or("-"),
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

// ── Runners ─────────────────────────────────────────────────────────

async fn cmd_runners_list(client: &OxClient, json: bool) -> Result<()> {
    let resp = client.status().await?; // pool state from status for now
    // Use the raw pool state endpoint
    let pool: serde_json::Value = reqwest::Client::new()
        .get(format!("{}/api/state/pool", client.base_url()))
        .send()
        .await?
        .json()
        .await?;

    if json {
        println!("{}", serde_json::to_string_pretty(&pool)?);
    } else {
        let _ = resp;
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
            pretty_print_claude_log(&text);
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
                    pretty_print_claude_log(new_data);
                } else {
                    print!("{new_data}");
                }
                known_len = text.len();
            }
    }
}

/// Pretty-print Claude Code stream-json log output.
/// Renders assistant text, tool calls, and tool results in a readable format.
fn pretty_print_claude_log(text: &str) {
    const DIM: &str = "\x1b[2m";
    const BOLD: &str = "\x1b[1m";
    const CYAN: &str = "\x1b[36m";
    const RED: &str = "\x1b[31m";
    const RESET: &str = "\x1b[0m";

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => {
                println!("{line}");
                continue;
            }
        };

        match v["type"].as_str().unwrap_or("") {
            "system" => {
                if v["subtype"].as_str() == Some("init") {
                    let model = v["model"].as_str().unwrap_or("?");
                    let cwd = v["cwd"].as_str().unwrap_or("?");
                    println!("{DIM}── session: model={model} cwd={cwd} ──{RESET}");
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
                                    println!("{BOLD}{text}{RESET}");
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
                                            println!("{CYAN}$ {cmd}{RESET}  {DIM}# {desc}{RESET}");
                                        } else {
                                            println!("{CYAN}$ {cmd}{RESET}");
                                        }
                                    }
                                    "Read" => {
                                        let path = input["file_path"].as_str().unwrap_or("?");
                                        println!("{CYAN}  read {path}{RESET}");
                                    }
                                    "Write" => {
                                        let path = input["file_path"].as_str().unwrap_or("?");
                                        println!("{CYAN}  write {path}{RESET}");
                                    }
                                    "Edit" => {
                                        let path = input["file_path"].as_str().unwrap_or("?");
                                        println!("{CYAN}  edit {path}{RESET}");
                                    }
                                    "Glob" => {
                                        let pattern = input["pattern"].as_str().unwrap_or("?");
                                        println!("{CYAN}  glob {pattern}{RESET}");
                                    }
                                    "Grep" => {
                                        let pattern = input["pattern"].as_str().unwrap_or("?");
                                        println!("{CYAN}  grep {pattern}{RESET}");
                                    }
                                    _ => {
                                        let short = serde_json::to_string(input).unwrap_or_default();
                                        let short = if short.len() > 80 {
                                            format!("{}...", &short[..80])
                                        } else {
                                            short
                                        };
                                        println!("{CYAN}  {name}({short}){RESET}");
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
                                println!("{RED}  error: {short}{RESET}");
                            } else if !content_str.is_empty() {
                                // Show first line of output, dimmed
                                let first_line = content_str.lines().next().unwrap_or("");
                                let first_line = if first_line.len() > 120 {
                                    format!("{}...", &first_line[..120])
                                } else {
                                    first_line.to_string()
                                };
                                println!("{DIM}  → {first_line}{RESET}");
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
                    println!(
                        "{DIM}── done: {:.1}s, {tokens_in}in/{tokens_out}out, ${cost:.4} ──{RESET}",
                        dur as f64 / 1000.0
                    );
                }
            }
            _ => {}
        }
    }
}

// ── Trigger ─────────────────────────────────────────────────────────

async fn cmd_trigger(client: &OxClient, node_id: &str, force: bool) -> Result<()> {
    let resp = client.trigger(node_id, force).await?;
    println!("{}", serde_json::to_string_pretty(&resp)?);
    Ok(())
}
