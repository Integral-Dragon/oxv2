use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, post, put},
};
use chrono::Utc;
use ox_core::events::*;
use ox_core::types::{ExecutionId, RunnerId};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::AppState;
use crate::artifacts;
use crate::cx;
use crate::db;
use crate::merge;
use crate::projections;

pub fn router() -> Router<AppState> {
    Router::new()
        // Status
        .route("/api/status", get(status))
        // Runners
        .route("/api/runners/register", post(register_runner))
        .route("/api/runners/{id}/heartbeat", post(heartbeat))
        .route("/api/runners/{id}/drain", post(drain_runner))
        // State projections
        .route("/api/state/pool", get(get_pool_state))
        .route("/api/state/executions", get(get_executions_state))
        .route("/api/state/cx", get(get_cx_state))
        // Secrets
        .route("/api/secrets", get(list_secrets))
        .route("/api/secrets/{name}", put(set_secret).delete(delete_secret))
        // Workflows
        .route("/api/workflows", get(list_workflows))
        // Executions
        .route("/api/executions", get(list_executions).post(create_execution))
        .route("/api/executions/{id}", get(get_execution))
        .route("/api/executions/{id}/cancel", post(cancel_execution))
        .route(
            "/api/executions/{id}/complete",
            post(complete_execution),
        )
        .route(
            "/api/executions/{id}/escalate",
            post(escalate_execution),
        )
        // Merge
        .route(
            "/api/executions/{id}/steps/{step}/merge",
            post(merge_step),
        )
        // Triggers
        .route("/api/triggers/evaluate", post(evaluate_triggers))
        // Metrics
        .route(
            "/api/executions/{id}/steps/{step}/metrics",
            get(get_step_metrics),
        )
        // Step logs
        .route(
            "/api/executions/{id}/steps/{step}/log/chunk",
            post(push_log_chunk),
        )
        .route(
            "/api/executions/{id}/steps/{step}/log",
            get(get_step_log),
        )
        // Steps
        .route(
            "/api/executions/{id}/steps/{step}/dispatch",
            post(dispatch_step),
        )
        .route(
            "/api/executions/{id}/steps/{step}/done",
            post(step_done),
        )
        .route(
            "/api/executions/{id}/steps/{step}/signals",
            post(step_signals),
        )
        .route(
            "/api/executions/{id}/steps/{step}/confirm",
            post(step_confirm),
        )
        .route(
            "/api/executions/{id}/steps/{step}/fail",
            post(step_fail),
        )
        .route(
            "/api/executions/{id}/steps/{step}/advance",
            post(step_advance),
        )
        // Artifacts
        .route(
            "/api/executions/{id}/steps/{step}/artifacts",
            get(list_step_artifacts),
        )
        .route(
            "/api/executions/{id}/steps/{step}/artifacts/{name}",
            get(get_artifact),
        )
        .route(
            "/api/executions/{id}/steps/{step}/artifacts/{name}/chunks",
            post(write_artifact_chunk),
        )
        .route(
            "/api/executions/{id}/steps/{step}/artifacts/{name}/close",
            post(close_artifact),
        )
}

// ── Status ──────────────────────────────────────────────────────────

#[derive(Serialize)]
struct StatusResponse {
    status: &'static str,
    pool_size: usize,
    pool_executing: usize,
    pool_idle: usize,
    executions_running: usize,
    workflows_loaded: usize,
    event_seq: u64,
}

async fn status(State(state): State<AppState>) -> Json<StatusResponse> {
    let pool = state.bus.projections.pool();
    let execs = state.bus.projections.executions();

    let pool_executing = pool
        .runners
        .values()
        .filter(|r| {
            r.status == projections::RunnerStatus::Executing
                || r.status == projections::RunnerStatus::Assigned
        })
        .count();

    let pool_idle = pool
        .runners
        .values()
        .filter(|r| r.status == projections::RunnerStatus::Idle)
        .count();

    let executions_running = execs
        .executions
        .values()
        .filter(|e| e.status == projections::ExecutionStatus::Running)
        .count();

    Json(StatusResponse {
        status: "healthy",
        pool_size: pool.runners.len(),
        pool_executing,
        pool_idle,
        executions_running,
        workflows_loaded: state.workflows.len(),
        event_seq: state.bus.current_seq(),
    })
}

// ── Runners ─────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct RegisterRequest {
    #[serde(default = "default_environment")]
    environment: String,
    #[serde(default)]
    labels: HashMap<String, String>,
}

fn default_environment() -> String {
    "default".into()
}

#[derive(Serialize)]
struct RegisterResponse {
    runner_id: RunnerId,
}

async fn register_runner(
    State(state): State<AppState>,
    Json(req): Json<RegisterRequest>,
) -> (StatusCode, Json<RegisterResponse>) {
    let runner_id = RunnerId::generate();

    let data = RunnerRegisteredData {
        runner_id: runner_id.clone(),
        environment: req.environment,
        labels: req.labels,
    };

    state
        .bus
        .append(EventType::RunnerRegistered, serde_json::to_value(data).unwrap())
        .unwrap();

    let ts = Utc::now().to_rfc3339();
    state.bus.with_conn(|conn| {
        db::upsert_runner_heartbeat(conn, &runner_id.0, &ts).unwrap();
    });

    (StatusCode::CREATED, Json(RegisterResponse { runner_id }))
}

async fn heartbeat(State(state): State<AppState>, Path(id): Path<String>) -> StatusCode {
    let ts = Utc::now().to_rfc3339();
    state.bus.with_conn(|conn| {
        db::upsert_runner_heartbeat(conn, &id, &ts).unwrap();
    });
    StatusCode::NO_CONTENT
}

async fn drain_runner(State(state): State<AppState>, Path(id): Path<String>) -> StatusCode {
    let data = RunnerDrainedData {
        runner_id: RunnerId(id.clone()),
        reason: "manual drain".into(),
    };

    state
        .bus
        .append(EventType::RunnerDrained, serde_json::to_value(data).unwrap())
        .unwrap();

    state.bus.with_conn(|conn| {
        db::remove_runner(conn, &id).unwrap();
    });

    StatusCode::NO_CONTENT
}

// ── Pool State ──────────────────────────────────────────────────────

async fn get_pool_state(State(state): State<AppState>) -> Json<serde_json::Value> {
    let pool = state.bus.projections.pool();

    let runners: Vec<serde_json::Value> = pool
        .runners
        .values()
        .map(|r| {
            serde_json::json!({
                "id": r.id.0,
                "environment": r.environment,
                "labels": r.labels,
                "status": format!("{:?}", r.status).to_lowercase(),
                "current_step": r.current_step.as_ref().map(|s| s.to_string()),
                "registered_at": r.registered_at.to_rfc3339(),
            })
        })
        .collect();

    Json(serde_json::json!({ "runners": runners }))
}

// ── Executions State ────────────────────────────────────────────────

async fn get_executions_state(State(state): State<AppState>) -> Json<serde_json::Value> {
    let execs = state.bus.projections.executions();
    Json(serde_json::to_value(
        execs
            .executions
            .values()
            .map(execution_summary)
            .collect::<Vec<_>>(),
    ).unwrap())
}

// ── Secrets ─────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct SetSecretRequest {
    value: String,
}

async fn set_secret(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(req): Json<SetSecretRequest>,
) -> StatusCode {
    let data = SecretSetData {
        name,
        value: req.value,
    };
    state
        .bus
        .append(EventType::SecretSet, serde_json::to_value(data).unwrap())
        .unwrap();
    StatusCode::NO_CONTENT
}

async fn list_secrets(State(state): State<AppState>) -> Json<serde_json::Value> {
    let secrets = state.bus.projections.secrets();
    let names: Vec<serde_json::Value> = secrets
        .secrets
        .keys()
        .map(|name| serde_json::json!({ "name": name }))
        .collect();
    Json(serde_json::Value::Array(names))
}

async fn delete_secret(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> StatusCode {
    let secrets = state.bus.projections.secrets();
    if !secrets.secrets.contains_key(&name) {
        return StatusCode::NOT_FOUND;
    }
    let data = SecretDeletedData { name };
    state
        .bus
        .append(EventType::SecretDeleted, serde_json::to_value(data).unwrap())
        .unwrap();
    StatusCode::NO_CONTENT
}

// ── Workflows ───────────────────────────────────────────────────────

async fn list_workflows(State(state): State<AppState>) -> Json<serde_json::Value> {
    let workflows: Vec<serde_json::Value> = state
        .workflows
        .iter()
        .map(|(name, engine)| {
            let step_names: Vec<&str> = engine.steps.keys().map(|s| s.as_str()).collect();
            serde_json::json!({
                "name": name,
                "steps": step_names,
                "triggers": engine.triggers,
            })
        })
        .collect();
    Json(serde_json::Value::Array(workflows))
}

// ── Executions ──────────────────────────────────────────────────────

#[derive(Deserialize, Default)]
struct ListExecutionsQuery {
    status: Option<String>,
    workflow: Option<String>,
    task: Option<String>,
}

async fn list_executions(
    State(state): State<AppState>,
    Query(query): Query<ListExecutionsQuery>,
) -> Json<serde_json::Value> {
    let execs = state.bus.projections.executions();

    let results: Vec<serde_json::Value> = execs
        .executions
        .values()
        .filter(|e| {
            if let Some(ref s) = query.status {
                let status_str = format!("{:?}", e.status).to_lowercase();
                if &status_str != s {
                    return false;
                }
            }
            if let Some(ref w) = query.workflow {
                if &e.workflow != w {
                    return false;
                }
            }
            if let Some(ref t) = query.task {
                if &e.task_id != t {
                    return false;
                }
            }
            true
        })
        .map(execution_summary)
        .collect();

    Json(serde_json::Value::Array(results))
}

fn execution_summary(e: &projections::ExecutionState) -> serde_json::Value {
    serde_json::json!({
        "id": e.id.0,
        "task_id": e.task_id,
        "workflow": e.workflow,
        "status": format!("{:?}", e.status).to_lowercase(),
        "current_step": e.current_step,
        "created_at": e.created_at.to_rfc3339(),
    })
}

#[derive(Deserialize)]
struct CreateExecutionRequest {
    task_id: String,
    workflow: String,
    #[serde(default = "default_trigger")]
    trigger: String,
}

fn default_trigger() -> String {
    "manual".into()
}

async fn create_execution(
    State(state): State<AppState>,
    Json(req): Json<CreateExecutionRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, Json<serde_json::Value>)> {
    // Validate workflow exists
    if !state.workflows.contains_key(&req.workflow) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": format!("unknown workflow: {}", req.workflow) })),
        ));
    }

    // Generate execution ID: {task_id}-e{N}
    let execs = state.bus.projections.executions();
    let n = execs
        .executions
        .values()
        .filter(|e| e.task_id == req.task_id)
        .count()
        + 1;
    let execution_id = ExecutionId(format!("{}-e{n}", req.task_id));

    let data = ExecutionCreatedData {
        execution_id: execution_id.clone(),
        task_id: req.task_id,
        workflow: req.workflow,
        trigger: req.trigger,
    };

    state
        .bus
        .append(
            EventType::ExecutionCreated,
            serde_json::to_value(data).unwrap(),
        )
        .unwrap();

    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({ "execution_id": execution_id })),
    ))
}

async fn get_execution(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let execs = state.bus.projections.executions();
    let exec = execs.executions.get(&id).ok_or(StatusCode::NOT_FOUND)?;

    let attempts: Vec<serde_json::Value> = exec
        .attempts
        .iter()
        .map(|a| {
            serde_json::json!({
                "step": a.step,
                "attempt": a.attempt,
                "runner_id": a.runner_id.as_ref().map(|r| &r.0),
                "status": format!("{:?}", a.status).to_lowercase(),
                "output": a.output,
                "signals": a.signals,
                "error": a.error,
                "transition": a.transition,
                "started_at": a.started_at.to_rfc3339(),
                "completed_at": a.completed_at.map(|t| t.to_rfc3339()),
            })
        })
        .collect();

    Ok(Json(serde_json::json!({
        "id": exec.id.0,
        "task_id": exec.task_id,
        "workflow": exec.workflow,
        "status": format!("{:?}", exec.status).to_lowercase(),
        "current_step": exec.current_step,
        "current_attempt": exec.current_attempt,
        "created_at": exec.created_at.to_rfc3339(),
        "attempts": attempts,
    })))
}

async fn cancel_execution(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> StatusCode {
    let data = ExecutionCancelledData {
        execution_id: ExecutionId(id),
        reason: "manual cancel".into(),
    };
    state
        .bus
        .append(
            EventType::ExecutionCancelled,
            serde_json::to_value(data).unwrap(),
        )
        .unwrap();
    StatusCode::NO_CONTENT
}

// ── Steps ───────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct StepPathParams {
    id: String,
    step: String,
}

#[derive(Deserialize)]
struct DispatchStepRequest {
    runner_id: RunnerId,
    attempt: u32,
    #[serde(default)]
    task_id: String,
    runtime: serde_json::Value,
    workspace: serde_json::Value,
    #[serde(default)]
    artifacts: Vec<ArtifactDecl>,
}

async fn dispatch_step(
    State(state): State<AppState>,
    Path(params): Path<StepPathParams>,
    Json(req): Json<DispatchStepRequest>,
) -> StatusCode {
    use ox_core::runtime::{collect_secret_refs, resolve_step_spec};
    use ox_core::workflow::RuntimeSpec;

    // Try to resolve the runtime spec if a runtime type is provided
    let step_runtime: Option<RuntimeSpec> = serde_json::from_value(req.runtime.clone()).ok();

    let (runtime_value, secret_refs) = if let Some(ref step_rt) = step_runtime {
        // Look up the runtime definition
        if let Some(runtime_def) = state.runtimes.get(&step_rt.runtime_type) {
            let secrets = state.bus.projections.secrets();

            // Build context variables
            let mut context_vars = std::collections::HashMap::new();
            context_vars.insert("task_id".to_string(), req.task_id.clone());
            context_vars.insert("workspace".to_string(), ".".to_string());

            let secret_refs = collect_secret_refs(runtime_def, step_rt);

            match resolve_step_spec(
                runtime_def,
                step_rt,
                &secrets.secrets,
                &state.search_path,
                &context_vars,
            ) {
                Ok(resolved) => {
                    // Wrap the original runtime with the resolved spec
                    let mut rt = req.runtime.clone();
                    if let Some(obj) = rt.as_object_mut() {
                        obj.insert(
                            "resolved".to_string(),
                            serde_json::to_value(&resolved).unwrap(),
                        );
                    } else {
                        rt = serde_json::json!({
                            "resolved": resolved,
                        });
                    }
                    (rt, secret_refs)
                }
                Err(e) => {
                    tracing::warn!(err = %e, "failed to resolve runtime spec, sending unresolved");
                    (req.runtime.clone(), secret_refs)
                }
            }
        } else {
            tracing::warn!(runtime_type = %step_rt.runtime_type, "unknown runtime type");
            (req.runtime.clone(), vec![])
        }
    } else {
        (req.runtime.clone(), vec![])
    };

    // Interpolate workspace fields (e.g. branch = "{task_id}")
    let workspace_value = interpolate_workspace(&req.workspace, &req.task_id);

    let data = StepDispatchedData {
        execution_id: ExecutionId(params.id),
        step: params.step,
        attempt: req.attempt,
        runner_id: req.runner_id,
        secret_refs,
        runtime: runtime_value,
        workspace: workspace_value,
        artifacts: req.artifacts,
    };

    state
        .bus
        .append(
            EventType::StepDispatched,
            serde_json::to_value(data).unwrap(),
        )
        .unwrap();
    StatusCode::NO_CONTENT
}

/// Interpolate `{task_id}` in workspace fields (e.g. branch).
fn interpolate_workspace(workspace: &serde_json::Value, task_id: &str) -> serde_json::Value {
    let s = serde_json::to_string(workspace).unwrap_or_default();
    let interpolated = s.replace("{task_id}", task_id);
    serde_json::from_str(&interpolated).unwrap_or_else(|_| workspace.clone())
}

#[derive(Deserialize)]
struct StepDoneRequest {
    attempt: u32,
    output: String,
}

async fn step_done(
    State(state): State<AppState>,
    Path(params): Path<StepPathParams>,
    Json(req): Json<StepDoneRequest>,
) -> StatusCode {
    let data = StepDoneData {
        execution_id: ExecutionId(params.id),
        step: params.step,
        attempt: req.attempt,
        output: req.output,
    };
    state
        .bus
        .append(EventType::StepDone, serde_json::to_value(data).unwrap())
        .unwrap();
    StatusCode::NO_CONTENT
}

#[derive(Deserialize)]
struct StepSignalsRequest {
    attempt: u32,
    signals: Vec<String>,
}

async fn step_signals(
    State(state): State<AppState>,
    Path(params): Path<StepPathParams>,
    Json(req): Json<StepSignalsRequest>,
) -> StatusCode {
    let data = StepSignalsData {
        execution_id: ExecutionId(params.id),
        step: params.step,
        attempt: req.attempt,
        signals: req.signals,
    };
    state
        .bus
        .append(EventType::StepSignals, serde_json::to_value(data).unwrap())
        .unwrap();
    StatusCode::NO_CONTENT
}

#[derive(Deserialize)]
struct StepConfirmRequest {
    attempt: u32,
    #[serde(default)]
    metrics: Option<serde_json::Value>,
}

async fn step_confirm(
    State(state): State<AppState>,
    Path(params): Path<StepPathParams>,
    Json(req): Json<StepConfirmRequest>,
) -> StatusCode {
    let data = StepConfirmedData {
        execution_id: ExecutionId(params.id),
        step: params.step,
        attempt: req.attempt,
        metrics: req.metrics,
    };
    state
        .bus
        .append(
            EventType::StepConfirmed,
            serde_json::to_value(data).unwrap(),
        )
        .unwrap();
    StatusCode::NO_CONTENT
}

#[derive(Deserialize)]
struct StepFailRequest {
    attempt: u32,
    error: String,
}

async fn step_fail(
    State(state): State<AppState>,
    Path(params): Path<StepPathParams>,
    Json(req): Json<StepFailRequest>,
) -> StatusCode {
    let data = StepFailedData {
        execution_id: ExecutionId(params.id),
        step: params.step,
        attempt: req.attempt,
        error: req.error,
    };
    state
        .bus
        .append(EventType::StepFailed, serde_json::to_value(data).unwrap())
        .unwrap();
    StatusCode::NO_CONTENT
}

#[derive(Deserialize)]
struct StepAdvanceRequest {
    from_step: String,
    to_step: String,
}

async fn step_advance(
    State(state): State<AppState>,
    Path(params): Path<StepPathParams>,
    Json(req): Json<StepAdvanceRequest>,
) -> StatusCode {
    let data = StepAdvancedData {
        execution_id: ExecutionId(params.id),
        from_step: req.from_step,
        to_step: req.to_step,
    };
    state
        .bus
        .append(
            EventType::StepAdvanced,
            serde_json::to_value(data).unwrap(),
        )
        .unwrap();
    StatusCode::NO_CONTENT
}

// ── cx State ───────────────────────────────────────────────────────

async fn get_cx_state(State(state): State<AppState>) -> Json<serde_json::Value> {
    let cx = state.bus.projections.cx();
    Json(serde_json::to_value(&cx).unwrap())
}

// ── Complete / Escalate ────────────────────────────────────────────

async fn complete_execution(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> StatusCode {
    let data = ExecutionCompletedData {
        execution_id: ExecutionId(id),
    };
    state
        .bus
        .append(
            EventType::ExecutionCompleted,
            serde_json::to_value(data).unwrap(),
        )
        .unwrap();
    StatusCode::NO_CONTENT
}

#[derive(Deserialize)]
struct EscalateRequest {
    step: String,
    reason: String,
}

async fn escalate_execution(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<EscalateRequest>,
) -> StatusCode {
    let data = ExecutionEscalatedData {
        execution_id: ExecutionId(id),
        step: req.step,
        reason: req.reason,
    };
    state
        .bus
        .append(
            EventType::ExecutionEscalated,
            serde_json::to_value(data).unwrap(),
        )
        .unwrap();
    StatusCode::NO_CONTENT
}

// ── Merge ──────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct MergeRequest {
    branch: String,
}

async fn merge_step(
    State(state): State<AppState>,
    Path(params): Path<StepPathParams>,
    Json(req): Json<MergeRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let execution_id = ExecutionId(params.id);

    match merge::merge_to_main(&state.repo_path, &req.branch) {
        Ok(result) => {
            let prev_head = result.prev_head.clone();
            let new_head = result.new_head.clone();

            // Emit git.merged event
            let merged_data = GitMergedData {
                branch: req.branch.clone(),
                into: "main".into(),
                sha: new_head.clone(),
                execution_id: execution_id.clone(),
            };
            state
                .bus
                .append(
                    EventType::GitMerged,
                    serde_json::to_value(merged_data).unwrap(),
                )
                .unwrap();

            // Derive and append cx events from the merge diff
            match cx::derive_cx_events_for_merge(&state.repo_path, &prev_head) {
                Ok(cx_events) => {
                    for cx_event in cx_events {
                        state
                            .bus
                            .append(cx_event.event_type, cx_event.data)
                            .unwrap();
                    }
                }
                Err(e) => {
                    tracing::warn!(err = %e, "failed to derive cx events from merge");
                }
            }

            Ok(Json(serde_json::json!({
                "status": "merged",
                "prev_head": prev_head,
                "new_head": new_head,
            })))
        }
        Err(e) => {
            // Emit git.merge_failed event
            let fail_data = GitMergeFailedData {
                branch: req.branch,
                into: "main".into(),
                reason: e.to_string(),
                execution_id,
            };
            state
                .bus
                .append(
                    EventType::GitMergeFailed,
                    serde_json::to_value(fail_data).unwrap(),
                )
                .unwrap();

            Err((
                StatusCode::CONFLICT,
                Json(serde_json::json!({ "error": e.to_string() })),
            ))
        }
    }
}

// ── Triggers ───────────────────────────────────────────────────────

#[derive(Deserialize)]
struct TriggerRequest {
    node_id: String,
    #[serde(default)]
    force: bool,
}

async fn evaluate_triggers(
    State(state): State<AppState>,
    Json(req): Json<TriggerRequest>,
) -> Json<serde_json::Value> {
    let cx = state.bus.projections.cx();
    let execs = state.bus.projections.executions();

    let node = cx.nodes.get(&req.node_id);

    // Check if node is shadowed (skip unless force)
    if !req.force {
        if let Some(n) = node {
            if n.shadowed {
                return Json(serde_json::json!({
                    "triggered": [],
                    "skipped": "node is shadowed",
                }));
            }
        }
    }

    let node_tags: Vec<String> = node.map(|n| n.tags.clone()).unwrap_or_default();

    let mut triggered = vec![];

    for (workflow_name, engine) in &state.workflows {
        for trigger in &engine.triggers {
            // Check event type match
            if trigger.on != "cx.task_ready" {
                continue;
            }

            // Check tag match
            if let Some(ref tag_pattern) = trigger.tag {
                if !node_tags.iter().any(|t| t == tag_pattern) {
                    continue;
                }
            }

            // Dedup check: is there already a running execution for this (task_id, workflow)?
            if !req.force {
                let already_running = execs.executions.values().any(|e| {
                    e.task_id == req.node_id
                        && e.workflow == *workflow_name
                        && e.status == projections::ExecutionStatus::Running
                });
                if already_running {
                    continue;
                }
            }

            // Create execution
            let n = execs
                .executions
                .values()
                .filter(|e| e.task_id == req.node_id)
                .count()
                + 1;
            let execution_id = ExecutionId(format!("{}-e{n}", req.node_id));

            let data = ExecutionCreatedData {
                execution_id: execution_id.clone(),
                task_id: req.node_id.clone(),
                workflow: workflow_name.clone(),
                trigger: trigger.on.clone(),
            };

            state
                .bus
                .append(
                    EventType::ExecutionCreated,
                    serde_json::to_value(data).unwrap(),
                )
                .unwrap();

            triggered.push(serde_json::json!({
                "execution_id": execution_id,
                "workflow": workflow_name,
            }));
        }
    }

    Json(serde_json::json!({ "triggered": triggered }))
}

// ── Metrics ────────────────────────────────────────────────────────

async fn get_step_metrics(
    State(state): State<AppState>,
    Path(params): Path<StepPathParams>,
) -> Json<serde_json::Value> {
    let execs = state.bus.projections.executions();
    let exec = match execs.executions.get(&params.id) {
        Some(e) => e,
        None => return Json(serde_json::json!(null)),
    };

    // Find the latest confirmed attempt for this step with metrics
    // We need to look at the event log for the actual metrics
    // For now, return the attempt info
    let attempts: Vec<serde_json::Value> = exec
        .attempts
        .iter()
        .filter(|a| a.step == params.step)
        .map(|a| {
            serde_json::json!({
                "step": a.step,
                "attempt": a.attempt,
                "status": format!("{:?}", a.status).to_lowercase(),
            })
        })
        .collect();

    Json(serde_json::json!({ "attempts": attempts }))
}

// ── Artifacts ───────────────────────────────────────────────────────

#[derive(Deserialize)]
struct ArtifactPathParams {
    id: String,
    step: String,
    name: String,
}

#[derive(Deserialize, Default)]
struct ArtifactQuery {
    attempt: Option<u32>,
}

async fn list_step_artifacts(
    State(state): State<AppState>,
    Path(params): Path<StepPathParams>,
    Query(query): Query<ArtifactQuery>,
) -> Json<serde_json::Value> {
    let result = state.bus.with_conn(|conn| {
        artifacts::list_artifacts(conn, &params.id, &params.step, query.attempt)
    });
    match result {
        Ok(arts) => Json(serde_json::to_value(arts).unwrap()),
        Err(_) => Json(serde_json::json!([])),
    }
}

async fn get_artifact(
    State(state): State<AppState>,
    Path(params): Path<ArtifactPathParams>,
    Query(query): Query<ArtifactQuery>,
) -> Result<axum::body::Bytes, StatusCode> {
    let attempt = query.attempt.unwrap_or(1);
    let result = state.bus.with_conn(|conn| {
        artifacts::fetch_artifact(conn, &params.id, &params.step, attempt, &params.name)
    });
    match result {
        Ok(data) => Ok(axum::body::Bytes::from(data)),
        Err(_) => Err(StatusCode::NOT_FOUND),
    }
}

#[derive(Deserialize)]
struct ChunkQuery {
    attempt: u32,
    offset: u64,
}

async fn write_artifact_chunk(
    State(state): State<AppState>,
    Path(params): Path<ArtifactPathParams>,
    Query(query): Query<ChunkQuery>,
    body: axum::body::Bytes,
) -> StatusCode {
    // Auto-declare if not yet declared
    state.bus.with_conn(|conn| {
        let _ = artifacts::declare_artifact(
            conn,
            &params.id,
            &params.step,
            query.attempt,
            &params.name,
            "file",
            true,
        );
        artifacts::write_chunk(
            conn,
            &params.id,
            &params.step,
            query.attempt,
            &params.name,
            query.offset,
            &body,
        )
        .unwrap();
    });
    StatusCode::NO_CONTENT
}

#[derive(Deserialize)]
struct CloseArtifactRequest {
    attempt: u32,
    size: u64,
    sha256: String,
}

async fn close_artifact(
    State(state): State<AppState>,
    Path(params): Path<ArtifactPathParams>,
    Json(req): Json<CloseArtifactRequest>,
) -> StatusCode {
    state.bus.with_conn(|conn| {
        artifacts::close_artifact(
            conn,
            &params.id,
            &params.step,
            req.attempt,
            &params.name,
            req.size,
            &req.sha256,
        )
        .unwrap();
    });

    // Emit artifact.closed event
    let data = ArtifactClosedData {
        execution_id: ExecutionId(params.id),
        step: params.step,
        artifact: params.name,
        size: req.size,
        sha256: req.sha256,
    };
    state
        .bus
        .append(
            EventType::ArtifactClosed,
            serde_json::to_value(data).unwrap(),
        )
        .unwrap();

    StatusCode::NO_CONTENT
}

// ── Step Logs ───────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct LogChunkPayload {
    attempt: u32,
    data: String,
}

/// POST /api/executions/{id}/steps/{step}/log/chunk — runner pushes log chunks
async fn push_log_chunk(
    State(state): State<AppState>,
    Path(params): Path<StepPathParams>,
    Json(payload): Json<LogChunkPayload>,
) -> StatusCode {
    if payload.data.is_empty() {
        return StatusCode::NO_CONTENT;
    }

    let log_path = step_log_path(&state.repo_path, &params.id, &params.step, payload.attempt);
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }

    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        Ok(mut file) => {
            use std::io::Write;
            if let Err(e) = file.write_all(payload.data.as_bytes()) {
                tracing::warn!(err = %e, "failed to write log chunk");
                return StatusCode::INTERNAL_SERVER_ERROR;
            }
        }
        Err(e) => {
            tracing::warn!(err = %e, "failed to open log file");
            return StatusCode::INTERNAL_SERVER_ERROR;
        }
    }

    StatusCode::NO_CONTENT
}

#[derive(Deserialize)]
struct LogQuery {
    attempt: Option<u32>,
    lines: Option<usize>,
}

/// GET /api/executions/{id}/steps/{step}/log — read step log
async fn get_step_log(
    State(state): State<AppState>,
    Path(params): Path<StepPathParams>,
    Query(q): Query<LogQuery>,
) -> axum::response::Response {
    use axum::response::IntoResponse;

    let log_path = if let Some(attempt) = q.attempt {
        step_log_path(&state.repo_path, &params.id, &params.step, attempt)
    } else {
        match find_most_recent_log(&state.repo_path, &params.id, &params.step) {
            Some(p) => p,
            None => {
                return (StatusCode::NOT_FOUND, "no logs found").into_response();
            }
        }
    };

    if !log_path.exists() {
        return (StatusCode::NOT_FOUND, "no logs found").into_response();
    }

    let contents = match std::fs::read_to_string(&log_path) {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to read log: {e}"),
            )
                .into_response();
        }
    };

    if let Some(n) = q.lines {
        let all_lines: Vec<&str> = contents.lines().collect();
        let start = all_lines.len().saturating_sub(n);
        let tail = all_lines[start..].join("\n");
        return (
            [(axum::http::header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            tail,
        )
            .into_response();
    }

    (
        [(axum::http::header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        contents,
    )
        .into_response()
}

fn step_log_path(
    repo_path: &std::path::Path,
    execution_id: &str,
    step: &str,
    attempt: u32,
) -> std::path::PathBuf {
    repo_path
        .join(".ox")
        .join("run")
        .join("logs")
        .join(execution_id)
        .join(format!("{step}-attempt-{attempt}.log"))
}

fn find_most_recent_log(
    repo_path: &std::path::Path,
    execution_id: &str,
    step: &str,
) -> Option<std::path::PathBuf> {
    let dir = repo_path
        .join(".ox")
        .join("run")
        .join("logs")
        .join(execution_id);
    if !dir.exists() {
        return None;
    }

    let prefix = format!("{step}-attempt-");
    let mut best: Option<(std::path::PathBuf, u32)> = None;

    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if let Some(rest) = name.strip_prefix(&prefix) {
                if let Some(num_str) = rest.strip_suffix(".log") {
                    if let Ok(n) = num_str.parse::<u32>() {
                        if best.as_ref().map(|(_, prev)| n > *prev).unwrap_or(true) {
                            best = Some((entry.path(), n));
                        }
                    }
                }
            }
        }
    }

    best.map(|(p, _)| p)
}
