use chrono::{DateTime, Utc};
use ox_core::events::*;
use ox_core::types::*;
use std::collections::HashMap;
use std::sync::RwLock;

// ── Pool Projection ─────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct PoolState {
    pub runners: HashMap<String, RunnerState>,
}

#[derive(Debug, Clone)]
pub struct RunnerState {
    pub id: RunnerId,
    pub environment: String,
    pub labels: HashMap<String, String>,
    pub status: RunnerStatus,
    pub current_step: Option<StepAttemptId>,
    pub registered_at: DateTime<Utc>,
    /// When the current step was dispatched to this runner.
    pub dispatched_at: Option<DateTime<Utc>>,
    /// Step timeout from the runtime spec (if set).
    pub step_timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunnerStatus {
    Idle,
    Assigned,
    Executing,
    Drained,
}

// ── Executions Projection ───────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct ExecutionsState {
    pub executions: HashMap<String, ExecutionState>,
}

#[derive(Debug, Clone)]
pub struct ExecutionState {
    pub id: ExecutionId,
    pub workflow: String,
    pub status: ExecutionStatus,
    pub vars: HashMap<String, String>,
    pub origin: ExecutionOrigin,
    pub attempts: Vec<StepAttemptState>,
    pub current_step: Option<String>,
    pub current_attempt: u32,
    pub visit_counts: HashMap<String, u32>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutionStatus {
    Running,
    Completed,
    Escalated,
    Cancelled,
}

#[derive(Debug, Clone)]
pub struct StepAttemptState {
    pub step: String,
    pub attempt: u32,
    pub runner_id: Option<RunnerId>,
    pub status: StepStatus,
    pub output: Option<String>,
    pub signals: Vec<String>,
    pub error: Option<String>,
    pub transition: Option<String>,
    pub connect_addr: Option<String>,
    pub started_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepStatus {
    Dispatched,
    Running,
    Done,
    Confirmed,
    Failed,
}

// ── Secrets Projection ──────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct SecretsState {
    pub secrets: HashMap<String, String>,
}

// ── cx Projection ──────────────────────────────────────────────────

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct CxState {
    pub nodes: HashMap<String, CxNodeState>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct CxNodeState {
    pub node_id: String,
    pub state: String,
    pub tags: Vec<String>,
    pub shadowed: bool,
    pub shadow_reason: Option<String>,
    pub comment_count: usize,
}

// ── Combined Projections ────────────────────────────────────────────

/// Thread-safe container for all projections.
pub struct Projections {
    pool: RwLock<PoolState>,
    executions: RwLock<ExecutionsState>,
    secrets: RwLock<SecretsState>,
    cx: RwLock<CxState>,
}

impl Default for Projections {
    fn default() -> Self {
        Self {
            pool: RwLock::new(PoolState::default()),
            executions: RwLock::new(ExecutionsState::default()),
            secrets: RwLock::new(SecretsState::default()),
            cx: RwLock::new(CxState::default()),
        }
    }
}

impl ExecutionState {
    /// Find the attempt entry matching (step, attempt), or create one if missing
    /// (e.g. for action steps that skip StepDispatched).
    fn find_or_create_attempt(&mut self, step: &str, attempt: u32, ts: DateTime<Utc>) -> &mut StepAttemptState {
        // Search backwards since recent attempts are at the end
        let pos = self.attempts.iter().rposition(|a| a.step == step && a.attempt == attempt);
        if let Some(idx) = pos {
            return &mut self.attempts[idx];
        }
        // Not found — create a synthetic entry (action step, no dispatch)
        self.attempts.push(StepAttemptState {
            step: step.to_string(),
            attempt,
            runner_id: None,
            status: StepStatus::Dispatched,
            output: None,
            signals: vec![],
            error: None,
            transition: None,
            connect_addr: None,
            started_at: ts,
            completed_at: None,
        });
        self.attempts.last_mut().unwrap()
    }
}

impl Projections {
    /// Apply an event to all projections.
    pub fn apply(&self, event: &EventEnvelope) {
        match event.event_type {
            // Runner events
            EventType::RunnerRegistered => {
                if let Ok(data) = serde_json::from_value::<RunnerRegisteredData>(event.data.clone())
                {
                    let mut pool = self.pool.write().unwrap();
                    pool.runners.insert(
                        data.runner_id.0.clone(),
                        RunnerState {
                            id: data.runner_id,
                            environment: data.environment,
                            labels: data.labels,
                            status: RunnerStatus::Idle,
                            current_step: None,
                            registered_at: event.ts,
                            dispatched_at: None,
                            step_timeout_secs: None,
                        },
                    );
                }
            }
            EventType::RunnerDrained => {
                if let Ok(data) = serde_json::from_value::<RunnerDrainedData>(event.data.clone()) {
                    let mut pool = self.pool.write().unwrap();
                    if let Some(runner) = pool.runners.get_mut(&data.runner_id.0) {
                        runner.status = RunnerStatus::Drained;
                    }
                }
            }

            // Step events that affect pool state
            EventType::StepDispatched => {
                if let Ok(data) =
                    serde_json::from_value::<StepDispatchedData>(event.data.clone())
                {
                    // Update runner status
                    let mut pool = self.pool.write().unwrap();
                    if let Some(runner) = pool.runners.get_mut(&data.runner_id.0) {
                        runner.status = RunnerStatus::Executing;
                        runner.current_step = Some(StepAttemptId {
                            execution_id: data.execution_id.clone(),
                            step: data.step.clone(),
                            attempt: data.attempt,
                        });
                        runner.dispatched_at = Some(event.ts);
                        runner.step_timeout_secs = data.runtime
                            .get("timeout")
                            .and_then(|v| v.as_u64());
                    }

                    // Update execution state. Re-dispatching the same
                    // (step, attempt) — e.g. after a heartbeat-missed orphan
                    // recovery — must update the existing attempt entry in
                    // place rather than push a phantom row.
                    let mut execs = self.executions.write().unwrap();
                    if let Some(exec) = execs.executions.get_mut(&data.execution_id.0) {
                        exec.current_step = Some(data.step.clone());
                        exec.current_attempt = data.attempt;
                        let attempt = exec.find_or_create_attempt(&data.step, data.attempt, event.ts);
                        attempt.runner_id = Some(data.runner_id);
                        attempt.status = StepStatus::Dispatched;
                        attempt.started_at = event.ts;
                        attempt.output = None;
                        attempt.signals.clear();
                        attempt.error = None;
                        attempt.transition = None;
                        attempt.connect_addr = None;
                        attempt.completed_at = None;
                    }
                }
            }
            EventType::StepRunning => {
                if let Ok(data) = serde_json::from_value::<StepRunningData>(event.data.clone()) {
                    let mut execs = self.executions.write().unwrap();
                    if let Some(exec) = execs.executions.get_mut(&data.execution_id.0) {
                        let attempt = exec.find_or_create_attempt(&data.step, data.attempt, event.ts);
                        attempt.status = StepStatus::Running;
                        if data.connect_addr.is_some() {
                            attempt.connect_addr = data.connect_addr;
                        }
                    }
                }
            }
            EventType::StepDone => {
                if let Ok(data) = serde_json::from_value::<StepDoneData>(event.data.clone()) {
                    let mut execs = self.executions.write().unwrap();
                    if let Some(exec) = execs.executions.get_mut(&data.execution_id.0) {
                        let attempt = exec.find_or_create_attempt(&data.step, data.attempt, event.ts);
                        attempt.status = StepStatus::Done;
                        attempt.output = Some(data.output);
                    }
                }
            }
            EventType::StepSignals => {
                if let Ok(data) = serde_json::from_value::<StepSignalsData>(event.data.clone()) {
                    let mut execs = self.executions.write().unwrap();
                    if let Some(exec) = execs.executions.get_mut(&data.execution_id.0) {
                        let attempt = exec.find_or_create_attempt(&data.step, data.attempt, event.ts);
                        attempt.signals = data.signals;
                    }
                }
            }
            EventType::StepConfirmed => {
                if let Ok(data) = serde_json::from_value::<StepConfirmedData>(event.data.clone()) {
                    // Return runner to idle
                    let mut pool = self.pool.write().unwrap();
                    for runner in pool.runners.values_mut() {
                        if let Some(ref step) = runner.current_step
                            && step.execution_id == data.execution_id && step.step == data.step {
                                runner.status = RunnerStatus::Idle;
                                runner.current_step = None;
                                runner.dispatched_at = None;
                                runner.step_timeout_secs = None;
                                break;
                            }
                    }

                    // Update step status
                    let mut execs = self.executions.write().unwrap();
                    if let Some(exec) = execs.executions.get_mut(&data.execution_id.0) {
                        let attempt = exec.find_or_create_attempt(&data.step, data.attempt, event.ts);
                        attempt.status = StepStatus::Confirmed;
                        attempt.completed_at = Some(event.ts);
                    }
                }
            }
            EventType::StepFailed => {
                if let Ok(data) = serde_json::from_value::<StepFailedData>(event.data.clone()) {
                    // Return runner to idle
                    let mut pool = self.pool.write().unwrap();
                    for runner in pool.runners.values_mut() {
                        if let Some(ref step) = runner.current_step
                            && step.execution_id == data.execution_id && step.step == data.step {
                                runner.status = RunnerStatus::Idle;
                                runner.current_step = None;
                                runner.dispatched_at = None;
                                runner.step_timeout_secs = None;
                                break;
                            }
                    }

                    // Update step status
                    let mut execs = self.executions.write().unwrap();
                    if let Some(exec) = execs.executions.get_mut(&data.execution_id.0) {
                        let attempt = exec.find_or_create_attempt(&data.step, data.attempt, event.ts);
                        attempt.status = StepStatus::Failed;
                        attempt.error = Some(data.error);
                        attempt.completed_at = Some(event.ts);
                    }
                }
            }
            EventType::StepTimeout => {
                if let Ok(data) = serde_json::from_value::<StepTimeoutData>(event.data.clone()) {
                    // Return runner to idle
                    let mut pool = self.pool.write().unwrap();
                    for runner in pool.runners.values_mut() {
                        if let Some(ref step) = runner.current_step
                            && step.execution_id == data.execution_id && step.step == data.step {
                                runner.status = RunnerStatus::Idle;
                                runner.current_step = None;
                                runner.dispatched_at = None;
                                runner.step_timeout_secs = None;
                                break;
                            }
                    }

                    // Update step status
                    let mut execs = self.executions.write().unwrap();
                    if let Some(exec) = execs.executions.get_mut(&data.execution_id.0) {
                        let attempt = exec.find_or_create_attempt(&data.step, data.attempt, event.ts);
                        attempt.status = StepStatus::Failed;
                        attempt.error = Some(format!("timeout after {}s", data.timeout_secs));
                        attempt.completed_at = Some(event.ts);
                    }
                }
            }
            EventType::StepAdvanced => {
                if let Ok(data) = serde_json::from_value::<StepAdvancedData>(event.data.clone()) {
                    let mut execs = self.executions.write().unwrap();
                    if let Some(exec) = execs.executions.get_mut(&data.execution_id.0) {
                        // StepAdvanced doesn't carry attempt number — find the latest
                        // attempt for from_step
                        if let Some(att) = exec.attempts.iter_mut().rfind(|a| a.step == data.from_step) {
                            att.transition = Some(data.to_step);
                        }
                    }
                }
            }

            // Execution lifecycle
            EventType::ExecutionCreated => {
                if let Ok(data) =
                    serde_json::from_value::<ExecutionCreatedData>(event.data.clone())
                {
                    let origin = data
                        .origin
                        .clone()
                        .unwrap_or_else(|| fallback_origin(&data.vars));
                    let mut execs = self.executions.write().unwrap();
                    execs.executions.insert(
                        data.execution_id.0.clone(),
                        ExecutionState {
                            id: data.execution_id,
                            workflow: data.workflow,
                            status: ExecutionStatus::Running,
                            vars: data.vars,
                            origin,
                            attempts: vec![],
                            current_step: None,
                            current_attempt: 0,
                            visit_counts: HashMap::new(),
                            created_at: event.ts,
                        },
                    );
                }
            }
            EventType::ExecutionCompleted => {
                if let Ok(data) =
                    serde_json::from_value::<ExecutionCompletedData>(event.data.clone())
                {
                    let mut execs = self.executions.write().unwrap();
                    if let Some(exec) = execs.executions.get_mut(&data.execution_id.0) {
                        exec.status = ExecutionStatus::Completed;
                    }
                }
            }
            EventType::ExecutionEscalated => {
                if let Ok(data) =
                    serde_json::from_value::<ExecutionEscalatedData>(event.data.clone())
                {
                    let mut execs = self.executions.write().unwrap();
                    if let Some(exec) = execs.executions.get_mut(&data.execution_id.0) {
                        exec.status = ExecutionStatus::Escalated;
                    }
                }
            }
            EventType::ExecutionCancelled => {
                if let Ok(data) =
                    serde_json::from_value::<ExecutionCancelledData>(event.data.clone())
                {
                    let mut execs = self.executions.write().unwrap();
                    if let Some(exec) = execs.executions.get_mut(&data.execution_id.0) {
                        exec.status = ExecutionStatus::Cancelled;
                    }
                }
            }

            // Secrets
            EventType::SecretSet => {
                if let Ok(data) = serde_json::from_value::<SecretSetData>(event.data.clone()) {
                    let mut secrets = self.secrets.write().unwrap();
                    secrets.secrets.insert(data.name, data.value);
                }
            }
            EventType::SecretDeleted => {
                if let Ok(data) = serde_json::from_value::<SecretDeletedData>(event.data.clone()) {
                    let mut secrets = self.secrets.write().unwrap();
                    secrets.secrets.remove(&data.name);
                }
            }

            // cx events
            EventType::CxTaskReady => {
                if let Ok(data) = serde_json::from_value::<CxTaskReadyData>(event.data.clone()) {
                    let mut cx = self.cx.write().unwrap();
                    let node = cx.nodes.entry(data.node_id.clone()).or_insert_with(|| {
                        CxNodeState {
                            node_id: data.node_id.clone(),
                            state: String::new(),
                            tags: vec![],
                            shadowed: false,
                            shadow_reason: None,
                            comment_count: 0,
                        }
                    });
                    node.state = "ready".into();
                    node.tags = data.tags;
                }
            }
            EventType::CxTaskClaimed => {
                if let Ok(data) = serde_json::from_value::<CxTaskClaimedData>(event.data.clone()) {
                    let mut cx = self.cx.write().unwrap();
                    if let Some(node) = cx.nodes.get_mut(&data.node_id) {
                        node.state = "claimed".into();
                    }
                }
            }
            EventType::CxTaskIntegrated => {
                if let Ok(data) =
                    serde_json::from_value::<CxTaskIntegratedData>(event.data.clone())
                {
                    let mut cx = self.cx.write().unwrap();
                    if let Some(node) = cx.nodes.get_mut(&data.node_id) {
                        node.state = "integrated".into();
                    }
                }
            }
            EventType::CxTaskShadowed => {
                if let Ok(data) =
                    serde_json::from_value::<CxTaskShadowedData>(event.data.clone())
                {
                    let mut cx = self.cx.write().unwrap();
                    if let Some(node) = cx.nodes.get_mut(&data.node_id) {
                        node.shadowed = true;
                        node.shadow_reason = Some(data.reason);
                    }
                }
            }
            EventType::CxCommentAdded => {
                if let Ok(data) =
                    serde_json::from_value::<CxCommentAddedData>(event.data.clone())
                {
                    let mut cx = self.cx.write().unwrap();
                    if let Some(node) = cx.nodes.get_mut(&data.node_id) {
                        node.comment_count += 1;
                    }
                }
            }

            // Other events don't update projections
            _ => {}
        }
    }

    pub fn pool(&self) -> PoolState {
        self.pool.read().unwrap().clone()
    }

    pub fn executions(&self) -> ExecutionsState {
        self.executions.read().unwrap().clone()
    }

    pub fn secrets(&self) -> SecretsState {
        self.secrets.read().unwrap().clone()
    }

    pub fn cx(&self) -> CxState {
        self.cx.read().unwrap().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn envelope(seq: u64, event_type: EventType, data: serde_json::Value) -> EventEnvelope {
        EventEnvelope {
            seq: Seq(seq),
            ts: Utc::now(),
            event_type,
            data,
        }
    }

    fn exec_created(seq: u64, exec_id: &str, workflow: &str) -> EventEnvelope {
        envelope(
            seq,
            EventType::ExecutionCreated,
            serde_json::to_value(ExecutionCreatedData {
                execution_id: ExecutionId(exec_id.into()),
                workflow: workflow.into(),
                trigger: "manual".into(),
                vars: HashMap::new(),
                origin: None,
            })
            .unwrap(),
        )
    }

    fn step_dispatched(seq: u64, exec_id: &str, step: &str, attempt: u32, runner: &str) -> EventEnvelope {
        envelope(
            seq,
            EventType::StepDispatched,
            serde_json::to_value(StepDispatchedData {
                execution_id: ExecutionId(exec_id.into()),
                step: step.into(),
                attempt,
                runner_id: RunnerId(runner.into()),
                secret_refs: vec![],
                runtime: serde_json::json!({}),
                workspace: serde_json::json!({}),
                artifacts: vec![],
            })
            .unwrap(),
        )
    }

    /// Re-dispatching the same (step, attempt) to a different runner
    /// must update the existing attempt entry in place, not push a phantom row.
    /// Reproduces the "two propose rows" bug observed via ox-ctl exec show.
    #[test]
    fn redispatch_same_attempt_updates_in_place() {
        let proj = Projections::default();
        proj.apply(&exec_created(1, "exec-1", "code-task"));
        proj.apply(&step_dispatched(2, "exec-1", "propose", 1, "run-0000"));
        proj.apply(&step_dispatched(3, "exec-1", "propose", 1, "run-0001"));

        let execs = proj.executions();
        let exec = execs.executions.get("exec-1").expect("execution exists");
        assert_eq!(exec.attempts.len(), 1, "re-dispatch should not push a new attempt");
        assert_eq!(exec.attempts[0].step, "propose");
        assert_eq!(exec.attempts[0].attempt, 1);
        assert_eq!(
            exec.attempts[0].runner_id,
            Some(RunnerId("run-0001".into())),
            "runner_id should reflect the latest dispatch"
        );
        assert_eq!(exec.attempts[0].status, StepStatus::Dispatched);
    }

    /// A new attempt number (workflow retry / loop) must still create a new row.
    /// This guards against over-collapsing when slice 1 is implemented.
    #[test]
    fn dispatch_new_attempt_pushes_new_row() {
        let proj = Projections::default();
        proj.apply(&exec_created(1, "exec-1", "code-task"));
        proj.apply(&step_dispatched(2, "exec-1", "propose", 1, "run-0000"));
        proj.apply(&step_dispatched(3, "exec-1", "propose", 2, "run-0001"));

        let execs = proj.executions();
        let exec = execs.executions.get("exec-1").expect("execution exists");
        assert_eq!(exec.attempts.len(), 2);
        assert_eq!(exec.attempts[0].attempt, 1);
        assert_eq!(exec.attempts[1].attempt, 2);
    }
}
