use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post, put},
};
use chrono::Utc;
use ox_core::events::*;
use ox_core::types::RunnerId;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::AppState;
use crate::db;

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
        // Secrets
        .route("/api/secrets", get(list_secrets))
        .route("/api/secrets/{name}", put(set_secret).delete(delete_secret))
}

// ── Status ──────────────────────────────────────────────────────────

#[derive(Serialize)]
struct StatusResponse {
    status: &'static str,
    pool_size: usize,
    pool_executing: usize,
    pool_idle: usize,
    executions_running: usize,
    event_seq: u64,
}

async fn status(State(bus): State<AppState>) -> Json<StatusResponse> {
    let pool = bus.projections.pool();
    let execs = bus.projections.executions();

    let pool_executing = pool
        .runners
        .values()
        .filter(|r| {
            r.status == crate::projections::RunnerStatus::Executing
                || r.status == crate::projections::RunnerStatus::Assigned
        })
        .count();

    let pool_idle = pool
        .runners
        .values()
        .filter(|r| r.status == crate::projections::RunnerStatus::Idle)
        .count();

    let executions_running = execs
        .executions
        .values()
        .filter(|e| e.status == crate::projections::ExecutionStatus::Running)
        .count();

    Json(StatusResponse {
        status: "healthy",
        pool_size: pool.runners.len(),
        pool_executing,
        pool_idle,
        executions_running,
        event_seq: bus.current_seq(),
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
    State(bus): State<AppState>,
    Json(req): Json<RegisterRequest>,
) -> (StatusCode, Json<RegisterResponse>) {
    let runner_id = RunnerId::generate();

    let data = RunnerRegisteredData {
        runner_id: runner_id.clone(),
        environment: req.environment,
        labels: req.labels,
    };

    bus.append(EventType::RunnerRegistered, serde_json::to_value(data).unwrap())
        .unwrap();

    // Insert initial heartbeat
    let ts = Utc::now().to_rfc3339();
    bus.with_conn(|conn| {
        db::upsert_runner_heartbeat(conn, &runner_id.0, &ts).unwrap();
    });

    (StatusCode::CREATED, Json(RegisterResponse { runner_id }))
}

async fn heartbeat(
    State(bus): State<AppState>,
    Path(id): Path<String>,
) -> StatusCode {
    let ts = Utc::now().to_rfc3339();
    bus.with_conn(|conn| {
        db::upsert_runner_heartbeat(conn, &id, &ts).unwrap();
    });
    StatusCode::NO_CONTENT
}

async fn drain_runner(
    State(bus): State<AppState>,
    Path(id): Path<String>,
) -> StatusCode {
    let data = RunnerDrainedData {
        runner_id: RunnerId(id.clone()),
        reason: "manual drain".into(),
    };

    bus.append(EventType::RunnerDrained, serde_json::to_value(data).unwrap())
        .unwrap();

    bus.with_conn(|conn| {
        db::remove_runner(conn, &id).unwrap();
    });

    StatusCode::NO_CONTENT
}

// ── Pool State ──────────────────────────────────────────────────────

#[derive(Serialize)]
struct PoolRunnerResponse {
    id: String,
    environment: String,
    labels: HashMap<String, String>,
    status: String,
    current_step: Option<String>,
    registered_at: String,
}

async fn get_pool_state(State(bus): State<AppState>) -> Json<serde_json::Value> {
    let pool = bus.projections.pool();

    let runners: Vec<PoolRunnerResponse> = pool
        .runners
        .values()
        .map(|r| PoolRunnerResponse {
            id: r.id.0.clone(),
            environment: r.environment.clone(),
            labels: r.labels.clone(),
            status: format!("{:?}", r.status).to_lowercase(),
            current_step: r.current_step.as_ref().map(|s| s.to_string()),
            registered_at: r.registered_at.to_rfc3339(),
        })
        .collect();

    Json(serde_json::json!({ "runners": runners }))
}

// ── Secrets ─────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct SetSecretRequest {
    value: String,
}

async fn set_secret(
    State(bus): State<AppState>,
    Path(name): Path<String>,
    Json(req): Json<SetSecretRequest>,
) -> StatusCode {
    let data = SecretSetData {
        name,
        value: req.value,
    };

    bus.append(EventType::SecretSet, serde_json::to_value(data).unwrap())
        .unwrap();

    StatusCode::NO_CONTENT
}

async fn list_secrets(State(bus): State<AppState>) -> Json<serde_json::Value> {
    let secrets = bus.projections.secrets();
    let names: Vec<serde_json::Value> = secrets
        .secrets
        .keys()
        .map(|name| serde_json::json!({ "name": name }))
        .collect();
    Json(serde_json::Value::Array(names))
}

async fn delete_secret(
    State(bus): State<AppState>,
    Path(name): Path<String>,
) -> StatusCode {
    let secrets = bus.projections.secrets();
    if !secrets.secrets.contains_key(&name) {
        return StatusCode::NOT_FOUND;
    }

    let data = SecretDeletedData { name };
    bus.append(
        EventType::SecretDeleted,
        serde_json::to_value(data).unwrap(),
    )
    .unwrap();

    StatusCode::NO_CONTENT
}
