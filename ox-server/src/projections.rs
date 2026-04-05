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
    pub task_id: String,
    pub workflow: String,
    pub status: ExecutionStatus,
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

// ── Combined Projections ────────────────────────────────────────────

/// Thread-safe container for all projections.
pub struct Projections {
    pool: RwLock<PoolState>,
    executions: RwLock<ExecutionsState>,
    secrets: RwLock<SecretsState>,
}

impl Default for Projections {
    fn default() -> Self {
        Self {
            pool: RwLock::new(PoolState::default()),
            executions: RwLock::new(ExecutionsState::default()),
            secrets: RwLock::new(SecretsState::default()),
        }
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
                    }

                    // Update execution state
                    let mut execs = self.executions.write().unwrap();
                    if let Some(exec) = execs.executions.get_mut(&data.execution_id.0) {
                        exec.current_step = Some(data.step.clone());
                        exec.current_attempt = data.attempt;
                        exec.attempts.push(StepAttemptState {
                            step: data.step,
                            attempt: data.attempt,
                            runner_id: Some(data.runner_id),
                            status: StepStatus::Dispatched,
                            output: None,
                            signals: vec![],
                            error: None,
                            transition: None,
                            started_at: event.ts,
                            completed_at: None,
                        });
                    }
                }
            }
            EventType::StepDone => {
                if let Ok(data) = serde_json::from_value::<StepDoneData>(event.data.clone()) {
                    let mut execs = self.executions.write().unwrap();
                    if let Some(exec) = execs.executions.get_mut(&data.execution_id.0) {
                        if let Some(attempt) = exec.attempts.last_mut() {
                            attempt.status = StepStatus::Done;
                            attempt.output = Some(data.output);
                        }
                    }
                }
            }
            EventType::StepSignals => {
                if let Ok(data) = serde_json::from_value::<StepSignalsData>(event.data.clone()) {
                    let mut execs = self.executions.write().unwrap();
                    if let Some(exec) = execs.executions.get_mut(&data.execution_id.0) {
                        if let Some(attempt) = exec.attempts.last_mut() {
                            attempt.signals = data.signals;
                        }
                    }
                }
            }
            EventType::StepConfirmed => {
                if let Ok(data) = serde_json::from_value::<StepConfirmedData>(event.data.clone()) {
                    // Return runner to idle
                    let mut pool = self.pool.write().unwrap();
                    for runner in pool.runners.values_mut() {
                        if let Some(ref step) = runner.current_step {
                            if step.execution_id == data.execution_id && step.step == data.step {
                                runner.status = RunnerStatus::Idle;
                                runner.current_step = None;
                                break;
                            }
                        }
                    }

                    // Update step status
                    let mut execs = self.executions.write().unwrap();
                    if let Some(exec) = execs.executions.get_mut(&data.execution_id.0) {
                        if let Some(attempt) = exec.attempts.last_mut() {
                            attempt.status = StepStatus::Confirmed;
                            attempt.completed_at = Some(event.ts);
                        }
                    }
                }
            }
            EventType::StepFailed => {
                if let Ok(data) = serde_json::from_value::<StepFailedData>(event.data.clone()) {
                    // Return runner to idle
                    let mut pool = self.pool.write().unwrap();
                    for runner in pool.runners.values_mut() {
                        if let Some(ref step) = runner.current_step {
                            if step.execution_id == data.execution_id && step.step == data.step {
                                runner.status = RunnerStatus::Idle;
                                runner.current_step = None;
                                break;
                            }
                        }
                    }

                    // Update step status
                    let mut execs = self.executions.write().unwrap();
                    if let Some(exec) = execs.executions.get_mut(&data.execution_id.0) {
                        if let Some(attempt) = exec.attempts.last_mut() {
                            attempt.status = StepStatus::Failed;
                            attempt.error = Some(data.error);
                            attempt.completed_at = Some(event.ts);
                        }
                    }
                }
            }
            EventType::StepAdvanced => {
                if let Ok(data) = serde_json::from_value::<StepAdvancedData>(event.data.clone()) {
                    let mut execs = self.executions.write().unwrap();
                    if let Some(exec) = execs.executions.get_mut(&data.execution_id.0) {
                        if let Some(attempt) = exec.attempts.last_mut() {
                            attempt.transition = Some(data.to_step);
                        }
                    }
                }
            }

            // Execution lifecycle
            EventType::ExecutionCreated => {
                if let Ok(data) =
                    serde_json::from_value::<ExecutionCreatedData>(event.data.clone())
                {
                    let mut execs = self.executions.write().unwrap();
                    execs.executions.insert(
                        data.execution_id.0.clone(),
                        ExecutionState {
                            id: data.execution_id,
                            task_id: data.task_id,
                            workflow: data.workflow,
                            status: ExecutionStatus::Running,
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

            // Other events don't affect Phase 1 projections
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
}
