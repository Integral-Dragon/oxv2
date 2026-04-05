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
use crate::db;
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
        // Secrets
        .route("/api/secrets", get(list_secrets))
        .route("/api/secrets/{name}", put(set_secret).delete(delete_secret))
        // Workflows
        .route("/api/workflows", get(list_workflows))
        // Executions
        .route("/api/executions", get(list_executions).post(create_execution))
        .route("/api/executions/{id}", get(get_execution))
        .route("/api/executions/{id}/cancel", post(cancel_execution))
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
    let data = StepDispatchedData {
        execution_id: ExecutionId(params.id),
        step: params.step,
        attempt: req.attempt,
        runner_id: req.runner_id,
        secret_refs: vec![], // TODO: resolve in Phase 4 when full runtime resolution is implemented
        runtime: req.runtime,
        workspace: req.workspace,
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
