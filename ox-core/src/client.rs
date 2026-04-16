use anyhow::{Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
// All response types derive both Serialize and Deserialize for flexibility.
use std::collections::HashMap;

use crate::types::{ExecutionId, RunnerId};

/// Snapshot of a single cx node returned by `/api/state/cx`. Mirrors the
/// shape of `ox-server::projections::CxNodeState` but lives here so the
/// herder can deserialize it without depending on the server crate.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CxNodeSnapshot {
    pub node_id: String,
    pub state: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub shadowed: bool,
    #[serde(default)]
    pub shadow_reason: Option<String>,
    #[serde(default)]
    pub comment_count: usize,
}

/// Snapshot of the cx projection returned by `/api/state/cx`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct CxStateSnapshot {
    pub nodes: HashMap<String, CxNodeSnapshot>,
}

/// Trait abstracting the subset of the ox-server API the herder calls.
/// Production code uses [`OxClient`]; tests can substitute a mock impl.
#[allow(async_fn_in_trait)]
pub trait OxClientApi: Send + Sync {
    async fn status(&self) -> Result<StatusResponse>;
    async fn list_workflows(&self) -> Result<Vec<WorkflowEntry>>;

    async fn create_execution(
        &self,
        workflow: &str,
        trigger: &str,
        vars: HashMap<String, String>,
        origin: Option<crate::events::ExecutionOrigin>,
    ) -> Result<ExecutionId>;

    async fn complete_execution(&self, id: &str) -> Result<()>;
    async fn escalate_execution(&self, id: &str, step: &str, reason: &str) -> Result<()>;

    async fn dispatch_step(&self, params: &DispatchStepParams) -> Result<()>;
    async fn step_done(&self, execution_id: &str, step: &str, attempt: u32, output: &str) -> Result<()>;
    async fn step_confirm(&self, execution_id: &str, step: &str, attempt: u32, metrics: Option<serde_json::Value>) -> Result<()>;
    async fn step_fail(&self, execution_id: &str, step: &str, attempt: u32, error: &str) -> Result<()>;
    async fn step_advance(&self, execution_id: &str, step: &str, from_step: &str, to_step: &str) -> Result<()>;

    async fn drain_runner(&self, runner_id: &RunnerId) -> Result<()>;

    async fn merge_to_main(
        &self,
        execution_id: &str,
        step: &str,
        branch: &str,
        squash: bool,
    ) -> Result<serde_json::Value>;

    async fn post_trigger_failed(&self, data: &crate::events::TriggerFailedData) -> Result<()>;
}

/// HTTP client for the ox-server API.
pub struct OxClient {
    base_url: String,
    http: Client,
}

/// Filter parameters for `list_executions`. `None` means unfiltered.
#[derive(Debug, Default, Clone)]
pub struct ListExecutionsFilter {
    pub status: Option<String>,
    pub workflow: Option<String>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

/// Build the query URL for `GET /api/executions` with the given filter.
/// Pure function — extracted for unit testing.
pub fn build_list_executions_url(base_url: &str, filter: &ListExecutionsFilter) -> String {
    let base = base_url.trim_end_matches('/');
    let mut params: Vec<String> = Vec::new();
    if let Some(ref s) = filter.status {
        params.push(format!("status={s}"));
    }
    if let Some(ref w) = filter.workflow {
        params.push(format!("workflow={w}"));
    }
    if let Some(l) = filter.limit {
        params.push(format!("limit={l}"));
    }
    if let Some(o) = filter.offset {
        params.push(format!("offset={o}"));
    }
    if params.is_empty() {
        format!("{base}/api/executions")
    } else {
        format!("{base}/api/executions?{}", params.join("&"))
    }
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
    #[serde(default)]
    pub vars: HashMap<String, String>,
    /// Origin of this execution. `#[serde(default)]` lets pre-refactor
    /// server responses (which didn't include the field) deserialize as
    /// `Manual { user: None }` rather than erroring.
    #[serde(default = "default_manual_origin")]
    pub origin: crate::events::ExecutionOrigin,
    pub workflow: String,
    pub status: String,
    pub current_step: Option<String>,
    pub current_attempt: u32,
    pub created_at: String,
    pub attempts: Vec<StepAttemptDetail>,
}

fn default_manual_origin() -> crate::events::ExecutionOrigin {
    crate::events::ExecutionOrigin::Manual { user: None }
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
    workflow: String,
    trigger: String,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    vars: HashMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    origin: Option<crate::events::ExecutionOrigin>,
}

/// Parameters for dispatching a step to a runner.
pub struct DispatchStepParams {
    pub execution_id: String,
    pub step: String,
    pub runner_id: RunnerId,
    pub attempt: u32,
    pub vars: HashMap<String, String>,
    /// Persona name for this step (persona-primary path).
    pub persona: Option<String>,
    /// Step-level prompt.
    pub prompt: Option<String>,
    pub runtime: serde_json::Value,
    pub workspace: serde_json::Value,
}

#[derive(Serialize)]
struct DispatchRequest {
    runner_id: RunnerId,
    attempt: u32,
    vars: HashMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    persona: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt: Option<String>,
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
    #[serde(skip_serializing_if = "Vec::is_empty")]
    signal_matches: Vec<crate::events::SignalMatch>,
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

    // ── Watchers ───────────────────────────────────────────────────

    /// `GET /api/watchers` — list watcher cursor rows. Each entry is
    /// one row from the server's `watcher_cursors` table, suitable
    /// for rendering in `ox-ctl status` or a dashboard. Returns an
    /// empty vec when no watcher has posted yet (cold start).
    pub async fn list_watchers(&self) -> Result<serde_json::Value> {
        let resp = self.http.get(self.url("/api/watchers")).send().await?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("GET /api/watchers → {status}: {text}");
        }
        Ok(resp.json().await?)
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
        workflow: &str,
        trigger: &str,
        vars: HashMap<String, String>,
        origin: Option<crate::events::ExecutionOrigin>,
    ) -> Result<ExecutionId> {
        let resp: CreateExecutionResponse = self
            .http
            .post(self.url("/api/executions"))
            .json(&CreateExecutionRequest {
                workflow: workflow.to_string(),
                trigger: trigger.to_string(),
                vars,
                origin,
            })
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(resp.execution_id)
    }

    /// Append a `trigger.failed` event to the server's event log. Called
    /// by the herder when its local `build_vars` pass errors out.
    pub async fn post_trigger_failed(
        &self,
        data: &crate::events::TriggerFailedData,
    ) -> Result<()> {
        self.http
            .post(self.url("/api/triggers/failed"))
            .json(data)
            .send()
            .await?
            .error_for_status()?;
        Ok(())
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

    pub async fn list_executions(
        &self,
        filter: ListExecutionsFilter,
    ) -> Result<serde_json::Value> {
        let url = build_list_executions_url(&self.base_url, &filter);
        self.http
            .get(&url)
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

    pub async fn dispatch_step(&self, params: &DispatchStepParams) -> Result<()> {
        self.http
            .post(self.url(&format!(
                "/api/executions/{}/steps/{}/dispatch",
                params.execution_id, params.step
            )))
            .json(&DispatchRequest {
                runner_id: params.runner_id.clone(),
                attempt: params.attempt,
                vars: params.vars.clone(),
                persona: params.persona.clone(),
                prompt: params.prompt.clone(),
                runtime: params.runtime.clone(),
                workspace: params.workspace.clone(),
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
        connect_addr: Option<&str>,
    ) -> Result<()> {
        let mut body = serde_json::json!({ "attempt": attempt });
        if let Some(addr) = connect_addr {
            body["connect_addr"] = serde_json::Value::String(addr.to_string());
        }
        self.http
            .post(self.url(&format!(
                "/api/executions/{execution_id}/steps/{step}/running"
            )))
            .json(&body)
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
        signal_matches: Vec<crate::events::SignalMatch>,
    ) -> Result<()> {
        self.http
            .post(self.url(&format!(
                "/api/executions/{execution_id}/steps/{step}/signals"
            )))
            .json(&StepSignalsRequest {
                attempt,
                signals,
                signal_matches,
            })
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

    pub async fn reload_config(&self) -> Result<serde_json::Value> {
        self.http
            .post(self.url("/api/config/reload"))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
            .context("parsing reload response")
    }

    pub async fn check_config(&self) -> Result<serde_json::Value> {
        self.http
            .post(self.url("/api/config/check"))
            .send()
            .await?
            .json()
            .await
            .context("parsing config check response")
    }

}

impl OxClientApi for OxClient {
    async fn status(&self) -> Result<StatusResponse> {
        OxClient::status(self).await
    }
    async fn list_workflows(&self) -> Result<Vec<WorkflowEntry>> {
        OxClient::list_workflows(self).await
    }
    async fn create_execution(
        &self,
        workflow: &str,
        trigger: &str,
        vars: HashMap<String, String>,
        origin: Option<crate::events::ExecutionOrigin>,
    ) -> Result<ExecutionId> {
        OxClient::create_execution(self, workflow, trigger, vars, origin).await
    }
    async fn complete_execution(&self, id: &str) -> Result<()> {
        OxClient::complete_execution(self, id).await
    }
    async fn escalate_execution(&self, id: &str, step: &str, reason: &str) -> Result<()> {
        OxClient::escalate_execution(self, id, step, reason).await
    }
    async fn dispatch_step(&self, params: &DispatchStepParams) -> Result<()> {
        OxClient::dispatch_step(self, params).await
    }
    async fn step_done(&self, execution_id: &str, step: &str, attempt: u32, output: &str) -> Result<()> {
        OxClient::step_done(self, execution_id, step, attempt, output).await
    }
    async fn step_confirm(&self, execution_id: &str, step: &str, attempt: u32, metrics: Option<serde_json::Value>) -> Result<()> {
        OxClient::step_confirm(self, execution_id, step, attempt, metrics).await
    }
    async fn step_fail(&self, execution_id: &str, step: &str, attempt: u32, error: &str) -> Result<()> {
        OxClient::step_fail(self, execution_id, step, attempt, error).await
    }
    async fn step_advance(&self, execution_id: &str, step: &str, from_step: &str, to_step: &str) -> Result<()> {
        OxClient::step_advance(self, execution_id, step, from_step, to_step).await
    }
    async fn drain_runner(&self, runner_id: &RunnerId) -> Result<()> {
        OxClient::drain_runner(self, runner_id).await
    }
    async fn merge_to_main(
        &self,
        execution_id: &str,
        step: &str,
        branch: &str,
        squash: bool,
    ) -> Result<serde_json::Value> {
        OxClient::merge_to_main(self, execution_id, step, branch, squash).await
    }
    async fn post_trigger_failed(&self, data: &crate::events::TriggerFailedData) -> Result<()> {
        OxClient::post_trigger_failed(self, data).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_url_no_filters() {
        let f = ListExecutionsFilter::default();
        let url = build_list_executions_url("http://ox.local", &f);
        assert_eq!(url, "http://ox.local/api/executions");
    }

    #[test]
    fn build_url_with_status() {
        let f = ListExecutionsFilter {
            status: Some("running".into()),
            ..Default::default()
        };
        let url = build_list_executions_url("http://ox.local", &f);
        assert_eq!(url, "http://ox.local/api/executions?status=running");
    }

    #[test]
    fn build_url_with_workflow() {
        let f = ListExecutionsFilter {
            workflow: Some("consultation".into()),
            ..Default::default()
        };
        let url = build_list_executions_url("http://ox.local", &f);
        assert_eq!(url, "http://ox.local/api/executions?workflow=consultation");
    }

    #[test]
    fn build_url_with_status_workflow_limit() {
        let f = ListExecutionsFilter {
            status: Some("running".into()),
            workflow: Some("consultation".into()),
            limit: Some(10),
            offset: None,
        };
        let url = build_list_executions_url("http://ox.local", &f);
        // Order follows declaration order: status, workflow, limit, offset.
        assert_eq!(
            url,
            "http://ox.local/api/executions?status=running&workflow=consultation&limit=10"
        );
    }

    #[test]
    fn build_url_strips_trailing_slash_from_base() {
        let f = ListExecutionsFilter::default();
        let url = build_list_executions_url("http://ox.local/", &f);
        assert_eq!(url, "http://ox.local/api/executions");
    }
}
