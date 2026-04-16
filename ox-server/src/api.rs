use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, post, put},
};
use ox_core::events::*;
use ox_core::types::{ExecutionId, RunnerId};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::AppState;
use crate::artifacts;
use crate::events::{IngestBatch, IngestError, WatcherCursor};
use crate::merge;
use crate::pool;
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
        // Config
        .route("/api/config/reload", post(reload_config))
        .route("/api/config/check", post(check_config))
        // Watchers (source event ingest)
        .route("/api/watchers", get(list_watchers))
        .route("/api/watchers/{source}/cursor", get(get_watcher_cursor))
        .route("/api/events/ingest", post(ingest_batch))
        // Triggers
        .route("/api/triggers/failed", post(post_trigger_failed))
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
            "/api/executions/{id}/steps/{step}/running",
            post(step_running),
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
        workflows_loaded: state.hot.load().workflows.len(),
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
    let runner_id = pool::register(&state.bus, req.environment, req.labels);
    (StatusCode::CREATED, Json(RegisterResponse { runner_id }))
}

#[derive(Deserialize, Default)]
struct HeartbeatRequest {
    #[serde(default)]
    execution_id: Option<String>,
    #[serde(default)]
    step: Option<String>,
    #[serde(default)]
    attempt: Option<u32>,
}

async fn heartbeat(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Option<Json<HeartbeatRequest>>,
) -> StatusCode {
    let req = body.map(|b| b.0).unwrap_or_default();
    pool::heartbeat(&state.bus, &id, req.execution_id.as_deref(), req.step.as_deref(), req.attempt);
    StatusCode::NO_CONTENT
}

async fn drain_runner(State(state): State<AppState>, Path(id): Path<String>) -> StatusCode {
    pool::drain(&state.bus, &id, "manual drain");
    StatusCode::NO_CONTENT
}

// ── Pool State ──────────────────────────────────────────────────────

async fn get_pool_state(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(pool::state(&state.bus))
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
    let subject = name.clone();
    let data = SecretSetData {
        name,
        value: req.value,
    };
    state
        .bus
        .append_ox(kinds::SECRET_SET, &subject, serde_json::to_value(data).unwrap())
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
    let subject = name.clone();
    let data = SecretDeletedData { name };
    state
        .bus
        .append_ox(kinds::SECRET_DELETED, &subject, serde_json::to_value(data).unwrap())
        .unwrap();
    StatusCode::NO_CONTENT
}

// ── Workflows ───────────────────────────────────────────────────────

async fn list_workflows(State(state): State<AppState>) -> Json<serde_json::Value> {
    let hot = state.hot.load();
    let workflows: Vec<serde_json::Value> = hot
        .workflows
        .iter()
        .map(|(name, engine)| {
            let step_names: Vec<&str> = engine.steps.keys().map(|s| s.as_str()).collect();
            // Collect triggers that target this workflow
            let workflow_triggers: Vec<&ox_core::workflow::TriggerDef> = hot
                .triggers
                .iter()
                .filter(|t| t.workflow == *name)
                .collect();
            serde_json::json!({
                "name": name,
                "steps": step_names,
                "triggers": workflow_triggers,
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
    limit: Option<usize>,
    offset: Option<usize>,
}

async fn list_executions(
    State(state): State<AppState>,
    Query(query): Query<ListExecutionsQuery>,
) -> Json<serde_json::Value> {
    let execs = state.bus.projections.executions();

    let mut results: Vec<_> = execs
        .executions
        .values()
        .filter(|e| {
            if let Some(ref s) = query.status {
                let status_str = format!("{:?}", e.status).to_lowercase();
                if &status_str != s {
                    return false;
                }
            }
            if let Some(ref w) = query.workflow
                && &e.workflow != w {
                    return false;
                }
            true
        })
        .collect::<Vec<_>>();

    results.sort_by(|a, b| b.created_at.cmp(&a.created_at));

    let total = results.len();
    let offset = query.offset.unwrap_or(0);
    let limit = query.limit.unwrap_or(25);
    let page: Vec<serde_json::Value> = results
        .iter()
        .skip(offset)
        .take(limit)
        .map(|e| execution_summary(e))
        .collect();

    Json(serde_json::json!({
        "executions": page,
        "total": total,
        "offset": offset,
        "limit": limit,
    }))
}

fn execution_summary(e: &projections::ExecutionState) -> serde_json::Value {
    serde_json::json!({
        "id": e.id.0,
        "vars": e.vars,
        "origin": e.origin,
        "workflow": e.workflow,
        "status": format!("{:?}", e.status).to_lowercase(),
        "current_step": e.current_step,
        "created_at": e.created_at.to_rfc3339(),
    })
}

#[derive(Deserialize)]
struct CreateExecutionRequest {
    workflow: String,
    #[serde(default = "default_trigger")]
    trigger: String,
    #[serde(default)]
    vars: HashMap<String, String>,
    /// Caller-supplied origin. Omitted by `ox-ctl exec run` style
    /// manual invocations (defaults to `Manual { user: None }`).
    #[serde(default)]
    origin: Option<ExecutionOrigin>,
}

fn default_trigger() -> String {
    "manual".into()
}

async fn create_execution(
    State(state): State<AppState>,
    Json(req): Json<CreateExecutionRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, Json<serde_json::Value>)> {
    // Validate workflow exists
    let hot = state.hot.load();
    let workflow = hot.workflows.get(&req.workflow).ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": format!("unknown workflow: {}", req.workflow) })),
        )
    })?;

    // Validate vars against workflow declarations
    let vars = workflow.validate_vars(&req.vars).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": e })),
        )
    })?;

    // Generate synthetic execution ID: e-{epoch_secs}-{seq}
    let epoch = chrono::Utc::now().timestamp();
    let seq = state.bus.current_seq() + 1;
    let execution_id = ExecutionId(format!("e-{epoch}-{seq}"));

    let data = ExecutionCreatedData {
        execution_id: execution_id.clone(),
        workflow: req.workflow,
        trigger: req.trigger,
        vars,
        origin: req.origin.unwrap_or(ExecutionOrigin::Manual { user: None }),
    };

    state
        .bus
        .append_ox(
            kinds::EXECUTION_CREATED,
            &execution_id.0,
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
        "vars": exec.vars,
        "origin": exec.origin,
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
    let subject = id.clone();
    let data = ExecutionCancelledData {
        execution_id: ExecutionId(id),
        reason: "manual cancel".into(),
    };
    state
        .bus
        .append_ox(
            kinds::EXECUTION_CANCELLED,
            &subject,
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
    vars: HashMap<String, String>,
    /// Persona name for this step (persona-primary path).
    #[serde(default)]
    persona: Option<String>,
    /// Step-level prompt.
    #[serde(default)]
    prompt: Option<String>,
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
    use ox_core::runtime::{collect_secret_refs, find_and_read_file, resolve_step_spec};
    use ox_core::workflow::{RuntimeSpec, VarType};

    let hot = state.hot.load();

    // Try to resolve the runtime spec if a runtime type is provided
    let mut step_runtime: Option<RuntimeSpec> = serde_json::from_value(req.runtime.clone()).ok();

    // ── Persona-primary resolution ─────────────────────────────────
    // If a persona name is provided, look it up and derive runtime/model from it.
    let persona_def = req.persona.as_ref().and_then(|name| hot.personas.get(name));

    if let Some(persona) = &persona_def {
        // Fill in runtime type from persona if the step didn't specify one.
        // A step may have [step.runtime] with just tty=true but no type.
        if let Some(rt_name) = &persona.runtime {
            match &mut step_runtime {
                None => {
                    step_runtime = Some(RuntimeSpec {
                        runtime: rt_name.clone(),
                        tty: false,
                        env: HashMap::new(),
                        timeout: None,
                        fields: HashMap::new(),
                    });
                }
                Some(rt) if rt.runtime.is_empty() => {
                    rt.runtime = rt_name.clone();
                }
                _ => {} // step already has an explicit runtime type
            }
        }

        // Inject persona vars as runtime field defaults (model, temperature, etc.)
        if let Some(step_rt) = &mut step_runtime {
            for (key, val) in &persona.vars {
                step_rt.fields.entry(key.clone())
                    .or_insert_with(|| toml::Value::String(val.clone()));
            }
        }

        // Inject step prompt into runtime fields if not already set
        if let (Some(prompt), Some(step_rt)) = (&req.prompt, &mut step_runtime) {
            step_rt.fields.entry("prompt".to_string())
                .or_insert_with(|| toml::Value::String(prompt.clone()));
        }
    } else if let Some(ref name) = req.persona {
        tracing::warn!(persona = %name, "persona not found, falling back to runtime spec");
    }

    let (runtime_value, secret_refs) = if let Some(ref step_rt) = step_runtime {
        // Look up the runtime definition
        if let Some(runtime_def) = hot.runtimes.get(&step_rt.runtime) {
            let secrets = state.bus.projections.secrets();

            // Build context variables.
            // Workflow/execution vars are prefixed with "workflow." to avoid
            // collisions with runtime vars (e.g. workflow.persona vs prompt).
            let mut context_vars: HashMap<String, String> = HashMap::new();
            context_vars.insert("workspace".to_string(), ".".to_string());

            // ── Persona context ────────────────────────────────────
            // Populate {persona.instructions} and {persona.name} for interpolation.
            if let Some(persona) = &persona_def {
                context_vars.insert("persona.instructions".to_string(), persona.instructions.clone());
                context_vars.insert("persona.name".to_string(), persona.name.clone());
            }

            // Resolve file-typed workflow vars and add all with "workflow." prefix
            let execs = state.bus.projections.executions();
            let workflow_name = execs.executions.get(&params.id)
                .map(|e| e.workflow.as_str());

            // Start with execution vars, merge step runtime overrides for workflow vars
            let mut workflow_vars = req.vars.clone();
            if let Some(wf) = workflow_name.and_then(|n| hot.workflows.get(n)) {
                // Step runtime fields can override workflow var values (e.g. persona per step)
                for name in wf.vars.keys() {
                    if let Some(val) = step_rt.fields.get(name) {
                        workflow_vars.insert(name.clone(), ox_core::runtime::toml_value_to_string(val));
                    }
                }

                // Resolve file-typed workflow vars to content
                for (name, def) in &wf.vars {
                    if def.var_type == VarType::File
                        && let Some(file_ref) = workflow_vars.get(name).cloned()
                        && !file_ref.is_empty()
                    {
                        let search_dir = def.search_dir.clone()
                            .unwrap_or_else(|| format!("{name}s"));
                        if let Some(content) = find_and_read_file(
                            &hot.search_path, &search_dir, &file_ref,
                        ) {
                            workflow_vars.insert(name.clone(), content);
                        } else {
                            tracing::warn!(var = %name, file = %file_ref, "file var not found on search path");
                        }
                    }
                }
            }

            // Backwards compat: if persona instructions resolved, also populate
            // workflow.persona so {workflow.persona} in runtime templates still works.
            if let Some(persona) = &persona_def {
                workflow_vars.entry("persona".to_string())
                    .or_insert_with(|| persona.instructions.clone());
            }

            // Add workflow vars with "workflow." prefix
            for (k, v) in &workflow_vars {
                context_vars.insert(format!("workflow.{k}"), v.clone());
            }

            let secret_refs = collect_secret_refs(runtime_def, step_rt);

            match resolve_step_spec(
                runtime_def,
                step_rt,
                &secrets.secrets,
                &hot.search_path,
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
            tracing::warn!(runtime = %step_rt.runtime, "unknown runtime");
            (req.runtime.clone(), vec![])
        }
    } else {
        (req.runtime.clone(), vec![])
    };

    // Interpolate vars in workspace fields (e.g. branch = "{task_id}")
    let workspace_value = interpolate_workspace(&req.workspace, &req.vars);

    let subject = params.id.clone();
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
        .append_ox(
            kinds::STEP_DISPATCHED,
            &subject,
            serde_json::to_value(data).unwrap(),
        )
        .unwrap();
    StatusCode::NO_CONTENT
}

/// Interpolate all `{varname}` placeholders in workspace fields.
fn interpolate_workspace(workspace: &serde_json::Value, vars: &HashMap<String, String>) -> serde_json::Value {
    let mut s = serde_json::to_string(workspace).unwrap_or_default();
    for (k, v) in vars {
        s = s.replace(&format!("{{{k}}}"), v);
    }
    serde_json::from_str(&s).unwrap_or_else(|_| workspace.clone())
}

#[derive(Deserialize)]
struct StepDoneRequest {
    attempt: u32,
    output: String,
}

#[derive(Deserialize)]
struct StepRunningRequest {
    attempt: u32,
    #[serde(default)]
    connect_addr: Option<String>,
}

async fn step_running(
    State(state): State<AppState>,
    Path(params): Path<StepPathParams>,
    Json(req): Json<StepRunningRequest>,
) -> StatusCode {
    let subject = params.id.clone();
    let data = StepRunningData {
        execution_id: ExecutionId(params.id),
        step: params.step,
        attempt: req.attempt,
        connect_addr: req.connect_addr,
    };
    state
        .bus
        .append_ox(kinds::STEP_RUNNING, &subject, serde_json::to_value(data).unwrap())
        .unwrap();
    StatusCode::NO_CONTENT
}

async fn step_done(
    State(state): State<AppState>,
    Path(params): Path<StepPathParams>,
    Json(req): Json<StepDoneRequest>,
) -> StatusCode {
    let subject = params.id.clone();
    let data = StepDoneData {
        execution_id: ExecutionId(params.id),
        step: params.step,
        attempt: req.attempt,
        output: req.output,
    };
    state
        .bus
        .append_ox(kinds::STEP_DONE, &subject, serde_json::to_value(data).unwrap())
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
    let subject = params.id.clone();
    let data = StepSignalsData {
        execution_id: ExecutionId(params.id),
        step: params.step,
        attempt: req.attempt,
        signals: req.signals,
    };
    state
        .bus
        .append_ox(kinds::STEP_SIGNALS, &subject, serde_json::to_value(data).unwrap())
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
    let subject = params.id.clone();
    let data = StepConfirmedData {
        execution_id: ExecutionId(params.id),
        step: params.step,
        attempt: req.attempt,
        metrics: req.metrics,
    };
    state
        .bus
        .append_ox(
            kinds::STEP_CONFIRMED,
            &subject,
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
    let subject = params.id.clone();
    let data = StepFailedData {
        execution_id: ExecutionId(params.id),
        step: params.step,
        attempt: req.attempt,
        error: req.error,
    };
    state
        .bus
        .append_ox(kinds::STEP_FAILED, &subject, serde_json::to_value(data).unwrap())
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
    let subject = params.id.clone();
    let data = StepAdvancedData {
        execution_id: ExecutionId(params.id),
        from_step: req.from_step,
        to_step: req.to_step,
    };
    state
        .bus
        .append_ox(
            kinds::STEP_ADVANCED,
            &subject,
            serde_json::to_value(data).unwrap(),
        )
        .unwrap();
    StatusCode::NO_CONTENT
}

// ── Complete / Escalate ────────────────────────────────────────────

async fn complete_execution(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> StatusCode {
    let subject = id.clone();
    let data = ExecutionCompletedData {
        execution_id: ExecutionId(id),
    };
    state
        .bus
        .append_ox(
            kinds::EXECUTION_COMPLETED,
            &subject,
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
    let subject = id.clone();
    let data = ExecutionEscalatedData {
        execution_id: ExecutionId(id),
        step: req.step,
        reason: req.reason,
    };
    state
        .bus
        .append_ox(
            kinds::EXECUTION_ESCALATED,
            &subject,
            serde_json::to_value(data).unwrap(),
        )
        .unwrap();
    StatusCode::NO_CONTENT
}

// ── Merge ──────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct MergeRequest {
    branch: String,
    #[serde(default)]
    squash: bool,
}

async fn merge_step(
    State(state): State<AppState>,
    Path(params): Path<StepPathParams>,
    Json(req): Json<MergeRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let execution_id = ExecutionId(params.id);

    match merge::merge_to_main(&state.repo_path, &req.branch, req.squash) {
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
                .append_ox(
                    kinds::GIT_MERGED,
                    &execution_id.0,
                    serde_json::to_value(merged_data).unwrap(),
                )
                .unwrap();

            // Source-specific side effects of the merge (cx node state
            // transitions, etc.) are observed by the corresponding
            // watcher on its next tick and arrive back as source events
            // through /api/events/ingest. The server does not derive
            // them here.

            Ok(Json(serde_json::json!({
                "status": "merged",
                "prev_head": prev_head,
                "new_head": new_head,
            })))
        }
        Err(e) => {
            // Emit git.merge_failed event
            let subject = execution_id.0.clone();
            let fail_data = GitMergeFailedData {
                branch: req.branch,
                into: "main".into(),
                reason: e.to_string(),
                execution_id,
            };
            state
                .bus
                .append_ox(
                    kinds::GIT_MERGE_FAILED,
                    &subject,
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

/// Append a `trigger.failed` event. Used by the herder when its own
/// `build_vars` pass errors before the server ever sees the request.
///
/// Manual triggering (the old `POST /api/triggers/evaluate` with a cx
/// node_id) is no longer supported server-side. To synthesize an event
/// by hand, post to `/api/events/ingest` directly or file a source
/// fact in the watched system.
async fn post_trigger_failed(
    State(state): State<AppState>,
    Json(data): Json<TriggerFailedData>,
) -> StatusCode {
    let subject = data.workflow.clone();
    match state
        .bus
        .append_ox(kinds::TRIGGER_FAILED, &subject, serde_json::to_value(data).unwrap())
    {
        Ok(_) => StatusCode::NO_CONTENT,
        Err(e) => {
            tracing::error!(err = %e, "failed to append trigger.failed event");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
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
    let subject = params.id.clone();
    let data = ArtifactClosedData {
        execution_id: ExecutionId(params.id),
        step: params.step,
        artifact: params.name,
        size: req.size,
        sha256: req.sha256,
    };
    state
        .bus
        .append_ox(
            kinds::ARTIFACT_CLOSED,
            &subject,
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
            if let Some(rest) = name.strip_prefix(&prefix)
                && let Some(num_str) = rest.strip_suffix(".log")
                    && let Ok(n) = num_str.parse::<u32>()
                        && best.as_ref().map(|(_, prev)| n > *prev).unwrap_or(true) {
                            best = Some((entry.path(), n));
                        }
        }
    }

    best.map(|(p, _)| p)
}

// ── Config Reload ──────────────────────────────────────────────────

async fn reload_config(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    match crate::state::HotConfig::load(&state.repo_path) {
        Ok(new) => {
            let summary = serde_json::json!({
                "status": "ok",
                "workflows": new.workflows.len(),
                "runtimes": new.runtimes.len(),
                "personas": new.personas.len(),
                "triggers": new.triggers.len(),
            });
            state.hot.store(std::sync::Arc::new(new));
            tracing::info!("config reloaded via API");
            Ok(Json(summary))
        }
        Err(e) => Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({
                "status": "error",
                "errors": [e.to_string()]
            })),
        )),
    }
}

async fn check_config(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    match crate::state::HotConfig::load(&state.repo_path) {
        Ok(new) => {
            let current = state.hot.load();

            let diff = |current_keys: Vec<&String>, new_keys: Vec<&String>| -> serde_json::Value {
                let current_set: std::collections::HashSet<_> = current_keys.into_iter().collect();
                let new_set: std::collections::HashSet<_> = new_keys.into_iter().collect();
                let mut added: Vec<_> = new_set.difference(&current_set).collect();
                let mut removed: Vec<_> = current_set.difference(&new_set).collect();
                added.sort();
                removed.sort();
                serde_json::json!({ "added": added, "removed": removed })
            };

            let changes = serde_json::json!({
                "workflows": diff(
                    current.workflows.keys().collect(),
                    new.workflows.keys().collect(),
                ),
                "runtimes": diff(
                    current.runtimes.keys().collect(),
                    new.runtimes.keys().collect(),
                ),
                "personas": diff(
                    current.personas.keys().collect(),
                    new.personas.keys().collect(),
                ),
            });

            Ok(Json(serde_json::json!({
                "valid": true,
                "changes": changes,
            })))
        }
        Err(e) => Ok(Json(serde_json::json!({
            "valid": false,
            "errors": [e.to_string()]
        }))),
    }
}

// ── Watchers (source event ingest) ─────────────────────────────────

#[derive(Serialize)]
struct WatcherCursorRow {
    source: String,
    cursor: Option<String>,
    updated_at: String,
    updated_seq: Option<u64>,
    last_error: Option<String>,
}

impl From<WatcherCursor> for WatcherCursorRow {
    fn from(c: WatcherCursor) -> Self {
        Self {
            source: c.source,
            cursor: c.cursor,
            updated_at: c
                .updated_at
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            updated_seq: c.updated_seq,
            last_error: c.last_error,
        }
    }
}

async fn list_watchers(
    State(state): State<AppState>,
) -> Result<Json<Vec<WatcherCursorRow>>, (StatusCode, Json<serde_json::Value>)> {
    match state.bus.list_watcher_cursors() {
        Ok(rows) => Ok(Json(rows.into_iter().map(WatcherCursorRow::from).collect())),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )),
    }
}

#[derive(Serialize)]
struct CursorResponse {
    cursor: Option<String>,
    updated_at: Option<String>,
}

async fn get_watcher_cursor(
    State(state): State<AppState>,
    Path(source): Path<String>,
) -> Result<Json<CursorResponse>, (StatusCode, Json<serde_json::Value>)> {
    match state.bus.get_watcher_cursor(&source) {
        Ok(Some(row)) => Ok(Json(CursorResponse {
            cursor: row.cursor,
            updated_at: Some(
                row.updated_at
                    .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            ),
        })),
        Ok(None) => Ok(Json(CursorResponse {
            cursor: None,
            updated_at: None,
        })),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )),
    }
}

#[derive(Deserialize)]
struct IngestBatchRequest {
    source: String,
    #[serde(default)]
    cursor_before: Option<String>,
    cursor_after: String,
    #[serde(default)]
    events: Vec<IngestEventData>,
}

#[derive(Serialize)]
struct IngestBatchResponse {
    appended: u32,
    deduped: u32,
}

async fn ingest_batch(
    State(state): State<AppState>,
    Json(req): Json<IngestBatchRequest>,
) -> Result<Json<IngestBatchResponse>, (StatusCode, Json<serde_json::Value>)> {
    let batch = IngestBatch {
        source: req.source,
        cursor_before: req.cursor_before,
        cursor_after: req.cursor_after,
        events: req.events,
    };
    match state.bus.ingest_batch(batch) {
        Ok(result) => Ok(Json(IngestBatchResponse {
            appended: result.appended,
            deduped: result.deduped,
        })),
        Err(IngestError::CursorConflict { expected, actual }) => Err((
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": "cursor_before mismatch",
                "expected": expected,
                "actual": actual,
            })),
        )),
        Err(IngestError::Storage(e)) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )),
    }
}
