use anyhow::{Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
// All response types derive both Serialize and Deserialize for flexibility.
use std::collections::HashMap;

use crate::types::{ExecutionId, RunnerId};

/// HTTP client for the ox-server API.
pub struct OxClient {
    base_url: String,
    http: Client,
}

// ── Response types ──────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
pub struct StatusResponse {
    pub status: String,
    pub pool_size: usize,
    pub pool_executing: usize,
    pub pool_idle: usize,
    pub executions_running: usize,
    pub workflows_loaded: usize,
    pub event_seq: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RegisterResponse {
    pub runner_id: RunnerId,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CreateExecutionResponse {
    pub execution_id: ExecutionId,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ExecutionDetail {
    pub id: String,
    pub task_id: String,
    pub workflow: String,
    pub status: String,
    pub current_step: Option<String>,
    pub current_attempt: u32,
    pub created_at: String,
    pub attempts: Vec<StepAttemptDetail>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct StepAttemptDetail {
    pub step: String,
    pub attempt: u32,
    pub runner_id: Option<String>,
    pub status: String,
    pub output: Option<String>,
    pub signals: Vec<String>,
    pub error: Option<String>,
    pub transition: Option<String>,
    pub started_at: String,
    pub completed_at: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SecretEntry {
    pub name: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WorkflowEntry {
    pub name: String,
    pub steps: Vec<String>,
}

// ── Request types ───────────────────────────────────────────────────

#[derive(Serialize)]
struct RegisterRequest {
    environment: String,
    labels: HashMap<String, String>,
}

#[derive(Serialize)]
struct CreateExecutionRequest {
    task_id: String,
    workflow: String,
    trigger: String,
}

#[derive(Serialize)]
struct DispatchRequest {
    runner_id: RunnerId,
    attempt: u32,
    task_id: String,
    runtime: serde_json::Value,
    workspace: serde_json::Value,
}

#[derive(Serialize)]
struct StepDoneRequest {
    attempt: u32,
    output: String,
}

#[derive(Serialize)]
struct StepSignalsRequest {
    attempt: u32,
    signals: Vec<String>,
}

#[derive(Serialize)]
struct StepConfirmRequest {
    attempt: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    metrics: Option<serde_json::Value>,
}

#[derive(Serialize)]
struct StepFailRequest {
    attempt: u32,
    error: String,
}

#[derive(Serialize)]
struct StepAdvanceRequest {
    from_step: String,
    to_step: String,
}

#[derive(Serialize)]
struct SetSecretRequest {
    value: String,
}

impl OxClient {
    pub fn new(base_url: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            http: Client::new(),
        }
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    // ── Status ──────────────────────────────────────────────────────

    pub async fn status(&self) -> Result<StatusResponse> {
        self.http
            .get(self.url("/api/status"))
            .send()
            .await?
            .json()
            .await
            .context("parsing status response")
    }

    // ── Runners ─────────────────────────────────────────────────────

    pub async fn register_runner(
        &self,
        environment: &str,
        labels: HashMap<String, String>,
    ) -> Result<RunnerId> {
        let resp: RegisterResponse = self
            .http
            .post(self.url("/api/runners/register"))
            .json(&RegisterRequest {
                environment: environment.to_string(),
                labels,
            })
            .send()
            .await?
            .json()
            .await?;
        Ok(resp.runner_id)
    }

    pub async fn heartbeat(
        &self,
        runner_id: &RunnerId,
        execution_id: Option<&str>,
        step: Option<&str>,
        attempt: Option<u32>,
    ) -> Result<()> {
        self.http
            .post(self.url(&format!("/api/runners/{}/heartbeat", runner_id.0)))
            .json(&serde_json::json!({
                "execution_id": execution_id,
                "step": step,
                "attempt": attempt,
            }))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    pub async fn drain_runner(&self, runner_id: &RunnerId) -> Result<()> {
        self.http
            .post(self.url(&format!("/api/runners/{}/drain", runner_id.0)))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    // ── Executions ──────────────────────────────────────────────────

    pub async fn create_execution(
        &self,
        task_id: &str,
        workflow: &str,
        trigger: &str,
    ) -> Result<ExecutionId> {
        let resp: CreateExecutionResponse = self
            .http
            .post(self.url("/api/executions"))
            .json(&CreateExecutionRequest {
                task_id: task_id.to_string(),
                workflow: workflow.to_string(),
                trigger: trigger.to_string(),
            })
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(resp.execution_id)
    }

    pub async fn get_execution(&self, id: &str) -> Result<ExecutionDetail> {
        self.http
            .get(self.url(&format!("/api/executions/{id}")))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
            .context("parsing execution detail")
    }

    pub async fn list_executions(&self) -> Result<Vec<serde_json::Value>> {
        self.http
            .get(self.url("/api/executions"))
            .send()
            .await?
            .json()
            .await
            .context("parsing execution list")
    }

    pub async fn cancel_execution(&self, id: &str) -> Result<()> {
        self.http
            .post(self.url(&format!("/api/executions/{id}/cancel")))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    pub async fn complete_execution(&self, id: &str) -> Result<()> {
        self.http
            .post(self.url(&format!("/api/executions/{id}/complete")))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    pub async fn escalate_execution(
        &self,
        id: &str,
        step: &str,
        reason: &str,
    ) -> Result<()> {
        self.http
            .post(self.url(&format!("/api/executions/{id}/escalate")))
            .json(&serde_json::json!({ "step": step, "reason": reason }))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    // ── Steps ───────────────────────────────────────────────────────

    pub async fn dispatch_step(
        &self,
        execution_id: &str,
        step: &str,
        runner_id: &RunnerId,
        attempt: u32,
        task_id: &str,
        runtime: serde_json::Value,
        workspace: serde_json::Value,
    ) -> Result<()> {
        self.http
            .post(self.url(&format!(
                "/api/executions/{execution_id}/steps/{step}/dispatch"
            )))
            .json(&DispatchRequest {
                runner_id: runner_id.clone(),
                attempt,
                task_id: task_id.to_string(),
                runtime,
                workspace,
            })
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    pub async fn push_log_chunk(
        &self,
        execution_id: &str,
        step: &str,
        attempt: u32,
        data: &str,
    ) -> Result<()> {
        self.http
            .post(self.url(&format!(
                "/api/executions/{execution_id}/steps/{step}/log/chunk"
            )))
            .json(&serde_json::json!({
                "attempt": attempt,
                "data": data,
            }))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    pub async fn step_running(
        &self,
        execution_id: &str,
        step: &str,
        attempt: u32,
    ) -> Result<()> {
        self.http
            .post(self.url(&format!(
                "/api/executions/{execution_id}/steps/{step}/running"
            )))
            .json(&serde_json::json!({ "attempt": attempt }))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    pub async fn step_done(
        &self,
        execution_id: &str,
        step: &str,
        attempt: u32,
        output: &str,
    ) -> Result<()> {
        self.http
            .post(self.url(&format!(
                "/api/executions/{execution_id}/steps/{step}/done"
            )))
            .json(&StepDoneRequest {
                attempt,
                output: output.to_string(),
            })
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    pub async fn step_signals(
        &self,
        execution_id: &str,
        step: &str,
        attempt: u32,
        signals: Vec<String>,
    ) -> Result<()> {
        self.http
            .post(self.url(&format!(
                "/api/executions/{execution_id}/steps/{step}/signals"
            )))
            .json(&StepSignalsRequest { attempt, signals })
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    pub async fn step_confirm(
        &self,
        execution_id: &str,
        step: &str,
        attempt: u32,
        metrics: Option<serde_json::Value>,
    ) -> Result<()> {
        self.http
            .post(self.url(&format!(
                "/api/executions/{execution_id}/steps/{step}/confirm"
            )))
            .json(&StepConfirmRequest { attempt, metrics })
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    pub async fn step_fail(
        &self,
        execution_id: &str,
        step: &str,
        attempt: u32,
        error: &str,
    ) -> Result<()> {
        self.http
            .post(self.url(&format!(
                "/api/executions/{execution_id}/steps/{step}/fail"
            )))
            .json(&StepFailRequest {
                attempt,
                error: error.to_string(),
            })
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    pub async fn step_advance(
        &self,
        execution_id: &str,
        _step: &str,
        from_step: &str,
        to_step: &str,
    ) -> Result<()> {
        self.http
            .post(self.url(&format!(
                "/api/executions/{execution_id}/steps/{from_step}/advance"
            )))
            .json(&StepAdvanceRequest {
                from_step: from_step.to_string(),
                to_step: to_step.to_string(),
            })
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    // ── Secrets ─────────────────────────────────────────────────────

    pub async fn set_secret(&self, name: &str, value: &str) -> Result<()> {
        self.http
            .put(self.url(&format!("/api/secrets/{name}")))
            .json(&SetSecretRequest {
                value: value.to_string(),
            })
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    pub async fn list_secrets(&self) -> Result<Vec<SecretEntry>> {
        self.http
            .get(self.url("/api/secrets"))
            .send()
            .await?
            .json()
            .await
            .context("parsing secrets list")
    }

    pub async fn delete_secret(&self, name: &str) -> Result<()> {
        self.http
            .delete(self.url(&format!("/api/secrets/{name}")))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    // ── Workflows ───────────────────────────────────────────────────

    pub async fn list_workflows(&self) -> Result<Vec<WorkflowEntry>> {
        self.http
            .get(self.url("/api/workflows"))
            .send()
            .await?
            .json()
            .await
            .context("parsing workflows list")
    }

    // ── Triggers ────────────────────────────────────────────────────

    pub async fn trigger(&self, node_id: &str, force: bool) -> Result<serde_json::Value> {
        self.http
            .post(self.url("/api/triggers/evaluate"))
            .json(&serde_json::json!({ "node_id": node_id, "force": force }))
            .send()
            .await?
            .json()
            .await
            .context("parsing trigger response")
    }

    // ── Merge ──────────────────────────────────────────────────────

    pub async fn merge_to_main(
        &self,
        execution_id: &str,
        step: &str,
        branch: &str,
        squash: bool,
    ) -> Result<serde_json::Value> {
        self.http
            .post(self.url(&format!(
                "/api/executions/{execution_id}/steps/{step}/merge"
            )))
            .json(&serde_json::json!({ "branch": branch, "squash": squash }))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
            .context("parsing merge response")
    }
}
