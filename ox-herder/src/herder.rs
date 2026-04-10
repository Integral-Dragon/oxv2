use anyhow::{Context, Result};
use futures_util::StreamExt;
use ox_core::client::OxClient;
use ox_core::events::*;
use ox_core::types::*;
use ox_core::workflow::{RetryDecision, RetryTracker, StepAdvance, WorkflowDef, WorkflowEngine};
use reqwest_eventsource::{Event as SseEvent, EventSource};
use std::collections::HashMap;
use std::time::{Duration, Instant};

// ── Execution phase state machine ──────────────────────────────────

/// What the scheduler needs to do for this execution.
#[derive(Debug, Clone)]
enum ExecPhase {
    /// Step is in-flight on a runner — nothing to do.
    AwaitingStep,
    /// Step confirmed — scheduler should advance the workflow.
    NeedsAdvance { step: String },
    /// Step failed — scheduler should retry or escalate.
    NeedsFailure { step: String, error: String },
    /// Next step determined — needs dispatching (or inline action).
    Ready { step: String, attempt: u32 },
    /// Terminal — completed, escalated, or cancelled.
    Done,
}

// ── Local state views ──────────────────────────────────────────────

/// Local view of a runner's state, rebuilt from SSE events.
#[derive(Debug)]
struct RunnerView {
    id: RunnerId,
    idle: bool,
    current_execution: Option<String>,
    current_step: Option<String>,
}

/// Local view of an execution, rebuilt from SSE events.
#[derive(Debug)]
struct ExecutionView {
    task_id: String,
    workflow: String,
    status: String, // "running", "completed", "escalated", "cancelled"
    phase: ExecPhase,
    visit_counts: HashMap<String, u32>,
    last_output: Option<String>,
    retry_tracker: RetryTracker,
}

/// A pending trigger to evaluate in the next scheduling pass.
#[derive(Debug)]
struct PendingTrigger {
    node_id: String,
    event_type: String,
    tags: Vec<String>,
}

pub struct Herder {
    client: OxClient,
    server_url: String,
    pool_target: usize,
    #[allow(dead_code)]
    heartbeat_grace: Duration,
    tick_interval: Duration,

    // Local state rebuilt from SSE
    runners: HashMap<String, RunnerView>,
    executions: HashMap<String, ExecutionView>,
    workflows: HashMap<String, WorkflowEngine>,
    pending_triggers: Vec<PendingTrigger>,
    last_seq: u64,
    /// True while replaying historical events — suppresses side effects.
    replaying: bool,
    /// The server's current seq at startup, used to detect end of replay.
    replay_target: u64,
    /// Track last-fired times for poll triggers.
    #[allow(dead_code)]
    poll_trigger_times: HashMap<(String, usize), Instant>,
}

impl Herder {
    pub fn new(
        server_url: &str,
        pool_target: usize,
        heartbeat_grace_secs: u64,
        tick_interval_secs: u64,
    ) -> Self {
        Self {
            client: OxClient::new(server_url),
            server_url: server_url.trim_end_matches('/').to_string(),
            pool_target,
            heartbeat_grace: Duration::from_secs(heartbeat_grace_secs),
            tick_interval: Duration::from_secs(tick_interval_secs),
            runners: HashMap::new(),
            executions: HashMap::new(),
            workflows: HashMap::new(),
            pending_triggers: Vec::new(),
            last_seq: 0,
            replaying: true,
            replay_target: 0,
            poll_trigger_times: HashMap::new(),
        }
    }

    pub async fn run(&mut self) -> Result<()> {
        self.load_workflows().await?;

        // Get current server seq so we know when replay is done
        match self.client.status().await {
            Ok(s) => {
                self.replay_target = s.event_seq;
                tracing::info!(replay_target = self.replay_target, "replay target set");
            }
            Err(e) => {
                tracing::warn!(err = %e, "couldn't get server status, replay detection disabled");
                self.replaying = false;
            }
        }

        // Connect to SSE with full replay
        let url = format!("{}/api/events/stream?last_event_id=0", self.server_url);
        let mut es = EventSource::get(&url);

        let mut tick = tokio::time::interval(self.tick_interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        tracing::info!("connected to SSE, entering event loop");

        let mut backoff_secs: u64 = 1;
        const MAX_BACKOFF: u64 = 30;

        loop {
            tokio::select! {
                Some(event) = es.next() => {
                    match event {
                        Ok(SseEvent::Open) => {
                            tracing::debug!("SSE connection opened");
                            backoff_secs = 1;
                        }
                        Ok(SseEvent::Message(msg)) => {
                            if let Err(e) = self.handle_sse_message(&msg.event, &msg.data).await {
                                tracing::warn!(err = %e, event = %msg.event, "error handling SSE event");
                            }
                            // Check if replay is done
                            if self.replaying && self.last_seq >= self.replay_target {
                                self.replaying = false;
                                tracing::info!(seq = self.last_seq, "replay complete, entering live mode");
                                self.schedule().await;
                            }
                        }
                        Err(reqwest_eventsource::Error::StreamEnded) => {
                            tracing::warn!("SSE stream ended, reconnecting...");
                            es = EventSource::get(format!(
                                "{}/api/events/stream?last_event_id={}",
                                self.server_url, self.last_seq
                            ));
                        }
                        Err(e) => {
                            tracing::warn!(err = %e, backoff = backoff_secs, "SSE error, reconnecting...");
                            tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
                            backoff_secs = (backoff_secs * 2).min(MAX_BACKOFF);
                            es = EventSource::get(format!(
                                "{}/api/events/stream?last_event_id={}",
                                self.server_url, self.last_seq
                            ));
                        }
                    }
                }
                _ = tick.tick() => {
                    self.on_tick().await;
                }
            }
        }
    }

    async fn load_workflows(&mut self) -> Result<()> {
        let workflows = self.client.list_workflows().await?;
        tracing::info!(count = workflows.len(), "loaded workflows from server");

        let search_path = ox_core::config::resolve_search_path(std::path::Path::new("."));
        for (name, path) in ox_core::config::load_all_configs(&search_path, "workflows") {
            match WorkflowDef::from_file(&path) {
                Ok(def) => {
                    tracing::info!(workflow = %name, "loaded workflow definition");
                    self.workflows.insert(def.name.clone(), WorkflowEngine::from_def(def));
                }
                Err(e) => {
                    tracing::warn!(workflow = %name, err = %e, "failed to load workflow");
                }
            }
        }

        Ok(())
    }

    // ── Event handlers — pure state updates ────────────────────────

    async fn handle_sse_message(&mut self, event_type: &str, data: &str) -> Result<()> {
        let envelope: EventEnvelope =
            serde_json::from_str(data).context("parsing SSE event data")?;
        self.last_seq = envelope.seq.0;

        match event_type {
            "runner.registered" => {
                let d: RunnerRegisteredData = serde_json::from_value(envelope.data)?;
                tracing::info!(runner = %d.runner_id, "runner registered");
                self.runners.insert(
                    d.runner_id.0.clone(),
                    RunnerView {
                        id: d.runner_id,
                        idle: true,
                        current_execution: None,
                        current_step: None,
                    },
                );
            }
            "runner.drained" => {
                let d: RunnerDrainedData = serde_json::from_value(envelope.data)?;
                tracing::info!(runner = %d.runner_id, "runner drained");
                self.runners.remove(&d.runner_id.0);
            }
            "runner.heartbeat_missed" => {
                let d: RunnerHeartbeatMissedData = serde_json::from_value(envelope.data)?;
                tracing::warn!(
                    runner = %d.runner_id,
                    last_seen = %d.last_seen,
                    execution_id = ?d.execution_id,
                    step = ?d.step,
                    "runner heartbeat missed"
                );

                // Re-ready the orphaned step if the runner was working on one
                if let (Some(exec_id), Some(step), Some(attempt)) = (&d.execution_id, &d.step, d.attempt)
                    && let Some(exec) = self.executions.get_mut(exec_id)
                        && matches!(exec.phase, ExecPhase::AwaitingStep) {
                            tracing::info!(exec = %exec_id, step = %step, attempt, "re-dispatching orphaned step");
                            exec.phase = ExecPhase::Ready { step: step.clone(), attempt };
                        }
                // Remove the dead runner
                self.runners.remove(&d.runner_id.0);
            }

            "execution.created" => {
                let d: ExecutionCreatedData = serde_json::from_value(envelope.data)?;
                tracing::info!(exec = %d.execution_id, task = %d.task_id, workflow = %d.workflow, "execution created");

                // Determine first step
                let first_step = self.workflows.get(&d.workflow)
                    .and_then(|e| e.first_step())
                    .map(|s| s.to_string())
                    .unwrap_or_default();

                self.executions.insert(
                    d.execution_id.0.clone(),
                    ExecutionView {
                        task_id: d.task_id,
                        workflow: d.workflow,
                        status: "running".into(),
                        phase: ExecPhase::Ready { step: first_step, attempt: 1 },
                        visit_counts: HashMap::new(),
                        last_output: None,
                        retry_tracker: RetryTracker::new(),
                    },
                );
            }
            "execution.completed" => {
                let d: ExecutionCompletedData = serde_json::from_value(envelope.data)?;
                if let Some(exec) = self.executions.get_mut(&d.execution_id.0) {
                    exec.status = "completed".into();
                    exec.phase = ExecPhase::Done;
                }
                tracing::info!(exec = %d.execution_id, "execution completed");
            }
            "execution.escalated" => {
                let d: ExecutionEscalatedData = serde_json::from_value(envelope.data)?;
                if let Some(exec) = self.executions.get_mut(&d.execution_id.0) {
                    exec.status = "escalated".into();
                    exec.phase = ExecPhase::Done;
                }
                tracing::info!(exec = %d.execution_id, step = %d.step, reason = %d.reason, "execution escalated");
            }
            "execution.cancelled" => {
                let d: ExecutionCancelledData = serde_json::from_value(envelope.data)?;
                if let Some(exec) = self.executions.get_mut(&d.execution_id.0) {
                    exec.status = "cancelled".into();
                    exec.phase = ExecPhase::Done;
                }
            }

            "step.dispatched" => {
                let d: StepDispatchedData = serde_json::from_value(envelope.data)?;
                // Mark runner as busy
                if let Some(runner) = self.runners.get_mut(&d.runner_id.0) {
                    runner.idle = false;
                    runner.current_execution = Some(d.execution_id.0.clone());
                    runner.current_step = Some(d.step.clone());
                }
                if let Some(exec) = self.executions.get_mut(&d.execution_id.0) {
                    if exec.status == "running" {
                        exec.phase = ExecPhase::AwaitingStep;
                    }
                    // During replay, reconstruct visit_counts from dispatches
                    if self.replaying {
                        *exec.visit_counts.entry(d.step.clone()).or_insert(0) += 1;
                    }
                }
            }
            "step.done" => {
                let d: StepDoneData = serde_json::from_value(envelope.data)?;
                if let Some(exec) = self.executions.get_mut(&d.execution_id.0)
                    && exec.status == "running" {
                        exec.last_output = Some(d.output);
                    }
            }
            "step.confirmed" => {
                let d: StepConfirmedData = serde_json::from_value(envelope.data)?;
                self.free_runner_for_step(&d.execution_id.0, &d.step);
                if let Some(exec) = self.executions.get_mut(&d.execution_id.0)
                    && exec.status == "running" {
                        tracing::info!(exec = %d.execution_id, step = %d.step, "step confirmed");
                        exec.retry_tracker.reset();
                        exec.phase = ExecPhase::NeedsAdvance { step: d.step };
                    }
            }
            "step.failed" => {
                let d: StepFailedData = serde_json::from_value(envelope.data)?;
                self.free_runner_for_step(&d.execution_id.0, &d.step);
                if let Some(exec) = self.executions.get_mut(&d.execution_id.0)
                    && exec.status == "running" {
                        tracing::warn!(exec = %d.execution_id, step = %d.step, error = %d.error, "step failed");
                        exec.phase = ExecPhase::NeedsFailure { step: d.step, error: d.error };
                    }
            }
            "step.timeout" => {
                let d: StepTimeoutData = serde_json::from_value(envelope.data)?;
                self.free_runner_for_step(&d.execution_id.0, &d.step);
                if let Some(exec) = self.executions.get_mut(&d.execution_id.0)
                    && exec.status == "running" {
                        tracing::warn!(exec = %d.execution_id, step = %d.step, timeout_secs = d.timeout_secs, "step timed out");
                        exec.phase = ExecPhase::NeedsFailure {
                            step: d.step,
                            error: format!("step timeout after {}s", d.timeout_secs),
                        };
                    }
            }
            "step.advanced" => {
                // Pure state — no action needed, scheduler already handled it
            }

            // cx events — queue for trigger evaluation
            "cx.task_ready" => {
                let d: CxTaskReadyData = serde_json::from_value(envelope.data)?;
                tracing::info!(node = %d.node_id, tags = ?d.tags, replaying = self.replaying, "cx.task_ready");
                if !self.replaying {
                    self.pending_triggers.push(PendingTrigger {
                        node_id: d.node_id,
                        event_type: "cx.task_ready".into(),
                        tags: d.tags,
                    });
                }
            }
            "cx.task_claimed" => {
                let d: CxTaskClaimedData = serde_json::from_value(envelope.data)?;
                tracing::info!(node = %d.node_id, "cx.task_claimed");
            }
            "cx.task_integrated" => {
                let d: CxTaskIntegratedData = serde_json::from_value(envelope.data)?;
                tracing::info!(node = %d.node_id, "cx.task_integrated");
            }
            "cx.task_shadowed" => {
                let d: CxTaskShadowedData = serde_json::from_value(envelope.data)?;
                tracing::info!(node = %d.node_id, reason = %d.reason, "cx.task_shadowed");
            }

            // Git events
            "git.merged" => {
                let d: GitMergedData = serde_json::from_value(envelope.data)?;
                tracing::info!(branch = %d.branch, sha = %d.sha, exec = %d.execution_id, "git.merged");
            }
            "git.merge_failed" => {
                let d: GitMergeFailedData = serde_json::from_value(envelope.data)?;
                tracing::warn!(branch = %d.branch, reason = %d.reason, exec = %d.execution_id, "git.merge_failed");
            }

            _ => {}
        }

        // After every state update, run the scheduler (live mode only)
        if !self.replaying {
            self.schedule().await;
        }

        Ok(())
    }

    // ── Helpers ────────────────────────────────────────────────────

    fn free_runner_for_step(&mut self, execution_id: &str, step: &str) {
        for runner in self.runners.values_mut() {
            if runner.current_execution.as_deref() == Some(execution_id)
                && runner.current_step.as_deref() == Some(step)
            {
                runner.idle = true;
                runner.current_execution = None;
                runner.current_step = None;
                break;
            }
        }
    }

    fn find_idle_runner(&self) -> Option<RunnerId> {
        self.runners
            .values()
            .find(|r| r.idle)
            .map(|r| r.id.clone())
    }

    /// Check if an execution already has a step in-flight on a runner.
    fn has_active_runner(&self, execution_id: &str) -> bool {
        self.runners.values().any(|r| {
            !r.idle && r.current_execution.as_deref() == Some(execution_id)
        })
    }

    // ── The Scheduler ──────────────────────────────────────────────

    async fn schedule(&mut self) {
        // Phase 1: Evaluate pending triggers
        self.evaluate_pending_triggers().await;

        // Phase 2: Process execution state machines (loop until stable)
        loop {
            let mut changed = false;

            let exec_ids: Vec<String> = self.executions.keys()
                .filter(|id| self.executions[*id].status == "running")
                .cloned()
                .collect();

            for exec_id in exec_ids {
                if self.process_execution(&exec_id).await {
                    changed = true;
                }
            }

            if !changed { break; }
        }

        // Phase 3: Dispatch — match Ready(runner step) to idle runners
        self.dispatch_ready_steps().await;
    }

    /// Process a single execution's current phase. Returns true if state changed.
    async fn process_execution(&mut self, exec_id: &str) -> bool {
        let phase = match self.executions.get(exec_id) {
            Some(e) => e.phase.clone(),
            None => return false,
        };

        match phase {
            ExecPhase::NeedsAdvance { step } => {
                self.do_advance(exec_id, &step).await;
                true
            }
            ExecPhase::NeedsFailure { step, error } => {
                self.do_failure(exec_id, &step, &error).await;
                true
            }
            ExecPhase::Ready { ref step, attempt } => {
                // Only process action steps here — runner steps wait for dispatch phase
                let is_action = self.is_action_step(exec_id, step);
                if is_action {
                    let step = step.clone();
                    self.do_action_step(exec_id, &step, attempt).await;
                    true
                } else {
                    false
                }
            }
            ExecPhase::AwaitingStep => {
                // Step in-flight — server will emit runner.heartbeat_missed
                // if the runner dies or stops working on the step.
                false
            }
            ExecPhase::Done => false,
        }
    }

    /// Advance workflow after a step is confirmed.
    async fn do_advance(&mut self, exec_id: &str, current_step: &str) {
        let (workflow_name, output, mut visit_counts) = {
            let exec = match self.executions.get(exec_id) {
                Some(e) => e,
                None => return,
            };
            (
                exec.workflow.clone(),
                exec.last_output.clone().unwrap_or_default(),
                exec.visit_counts.clone(),
            )
        };

        let engine = match self.workflows.get(&workflow_name) {
            Some(e) => e,
            None => {
                tracing::error!(workflow = %workflow_name, "unknown workflow for advance");
                return;
            }
        };

        let advance = engine.next_step(current_step, &output, &mut visit_counts);

        // Write visit counts back
        if let Some(exec) = self.executions.get_mut(exec_id) {
            exec.visit_counts = visit_counts;
        }

        match advance {
            StepAdvance::Goto(next_step) => {
                tracing::info!(exec = %exec_id, from = %current_step, to = %next_step, "advancing");
                // Emit advance event
                if let Err(e) = self
                    .client
                    .step_advance(exec_id, current_step, current_step, &next_step)
                    .await
                {
                    tracing::error!(err = %e, "failed to emit step.advanced");
                }
                if let Some(exec) = self.executions.get_mut(exec_id) {
                    exec.phase = ExecPhase::Ready { step: next_step, attempt: 1 };
                }
            }
            StepAdvance::Complete => {
                tracing::info!(exec = %exec_id, "workflow complete");
                if let Err(e) = self.client.complete_execution(exec_id).await {
                    tracing::error!(err = %e, "failed to complete execution");
                }
                if let Some(exec) = self.executions.get_mut(exec_id) {
                    exec.status = "completed".into();
                    exec.phase = ExecPhase::Done;
                }
            }
            StepAdvance::Escalate => {
                tracing::warn!(exec = %exec_id, step = %current_step, "escalating");
                if let Err(e) = self
                    .client
                    .escalate_execution(exec_id, current_step, "max visits exceeded or wildcard escalation")
                    .await
                {
                    tracing::error!(err = %e, "failed to escalate execution");
                }
                if let Some(exec) = self.executions.get_mut(exec_id) {
                    exec.status = "escalated".into();
                    exec.phase = ExecPhase::Done;
                }
            }
        }
    }

    /// Handle step failure — retry or escalate.
    async fn do_failure(&mut self, exec_id: &str, step: &str, _error: &str) {
        let (max_retries, on_fail) = {
            let exec = match self.executions.get(exec_id) {
                Some(e) => e,
                None => return,
            };
            let engine = match self.workflows.get(&exec.workflow) {
                Some(e) => e,
                None => return,
            };
            let step_def = match engine.steps.get(step) {
                Some(s) => s,
                None => return,
            };
            (step_def.max_retries, step_def.on_fail.clone())
        };

        let decision = {
            let exec = match self.executions.get_mut(exec_id) {
                Some(e) => e,
                None => return,
            };
            exec.retry_tracker.record_failure(step, max_retries)
        };

        match decision {
            RetryDecision::Retry { attempt } => {
                tracing::info!(exec = %exec_id, step = %step, attempt, "retrying step");
                if let Some(exec) = self.executions.get_mut(exec_id) {
                    exec.phase = ExecPhase::Ready { step: step.to_string(), attempt };
                }
            }
            RetryDecision::Exhausted => {
                match on_fail.as_deref() {
                    Some("escalate") | None => {
                        tracing::warn!(exec = %exec_id, step = %step, "retries exhausted, escalating");
                        if let Err(e) = self
                            .client
                            .escalate_execution(exec_id, step, "retries exhausted")
                            .await
                        {
                            tracing::error!(err = %e, "failed to escalate execution");
                        }
                        if let Some(exec) = self.executions.get_mut(exec_id) {
                            exec.status = "escalated".into();
                            exec.phase = ExecPhase::Done;
                        }
                    }
                    Some(goto_step) => {
                        // Check max_visits on the target step before jumping
                        let target_action = {
                            let exec = self.executions.get_mut(exec_id);
                            let engine_ref = self.workflows.get(
                                &exec.as_ref().map(|e| e.workflow.clone()).unwrap_or_default()
                            );
                            if let (Some(exec), Some(engine)) = (exec, engine_ref) {
                                let count = exec.visit_counts.entry(goto_step.to_string()).or_insert(0);
                                *count += 1;
                                if let Some(step_def) = engine.steps.get(goto_step) {
                                    if let Some(max) = step_def.max_visits {
                                        if *count > max {
                                            let fallback = step_def.max_visits_goto.clone();
                                            Some(fallback) // Some(Some(step)) or Some(None) = escalate
                                        } else {
                                            None // ok to proceed
                                        }
                                    } else {
                                        None
                                    }
                                } else {
                                    None
                                }
                            } else {
                                None
                            }
                        };

                        match target_action {
                            Some(Some(fallback_step)) => {
                                tracing::warn!(exec = %exec_id, step = %goto_step, to = %fallback_step, "on_fail target exceeded max_visits, redirecting");
                                if let Some(exec) = self.executions.get_mut(exec_id) {
                                    exec.phase = ExecPhase::Ready { step: fallback_step, attempt: 1 };
                                }
                            }
                            Some(None) => {
                                tracing::warn!(exec = %exec_id, step = %goto_step, "on_fail target exceeded max_visits, escalating");
                                if let Err(e) = self
                                    .client
                                    .escalate_execution(exec_id, goto_step, "on_fail target exceeded max_visits")
                                    .await
                                {
                                    tracing::error!(err = %e, "failed to escalate execution");
                                }
                                if let Some(exec) = self.executions.get_mut(exec_id) {
                                    exec.status = "escalated".into();
                                    exec.phase = ExecPhase::Done;
                                }
                            }
                            None => {
                                tracing::info!(exec = %exec_id, from = %step, to = %goto_step, "retries exhausted, jumping to on_fail");
                                if let Some(exec) = self.executions.get_mut(exec_id) {
                                    exec.phase = ExecPhase::Ready { step: goto_step.to_string(), attempt: 1 };
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    /// Check if a step is an action step (e.g. merge_to_main).
    fn is_action_step(&self, exec_id: &str, step: &str) -> bool {
        let workflow_name = match self.executions.get(exec_id) {
            Some(e) => &e.workflow,
            None => return false,
        };
        let engine = match self.workflows.get(workflow_name) {
            Some(e) => e,
            None => return false,
        };
        let step_def = match engine.steps.get(step) {
            Some(s) => s,
            None => return false,
        };
        step_def.action.is_some()
    }

    /// Execute an action step inline (e.g. merge_to_main).
    async fn do_action_step(&mut self, exec_id: &str, step: &str, attempt: u32) {
        let workflow_name = match self.executions.get(exec_id) {
            Some(e) => e.workflow.clone(),
            None => return,
        };
        let engine = match self.workflows.get(&workflow_name) {
            Some(e) => e,
            None => return,
        };
        let step_def = match engine.steps.get(step) {
            Some(s) => s.clone(),
            None => return,
        };

        match step_def.action.as_deref() {
            Some("merge_to_main") => {
                tracing::info!(exec = %exec_id, step = %step, attempt, "executing merge_to_main action");

                let task_id = match self.executions.get(exec_id) {
                    Some(e) => e.task_id.clone(),
                    None => return,
                };

                match self.client.merge_to_main(exec_id, step, &task_id, step_def.squash).await {
                    Ok(_) => {
                        tracing::info!(exec = %exec_id, "merge_to_main succeeded");
                        // Report done+confirm events for the action step
                        let _ = self.client.step_done(exec_id, step, attempt, "pass").await;
                        let _ = self.client.step_confirm(exec_id, step, attempt, None).await;
                        // Apply result to local state immediately
                        if let Some(exec) = self.executions.get_mut(exec_id) {
                            exec.last_output = Some("pass".into());
                            exec.retry_tracker.reset();
                            exec.phase = ExecPhase::NeedsAdvance { step: step.to_string() };
                        }
                    }
                    Err(e) => {
                        tracing::error!(err = %e, "merge_to_main failed");
                        let error = e.to_string();
                        let _ = self.client.step_fail(exec_id, step, attempt, &error).await;
                        // Apply result to local state immediately
                        if let Some(exec) = self.executions.get_mut(exec_id) {
                            exec.phase = ExecPhase::NeedsFailure { step: step.to_string(), error };
                        }
                    }
                }
            }
            Some(action) => {
                tracing::error!(exec = %exec_id, action, "unknown action step");
                let error = format!("unknown action: {action}");
                let _ = self.client.step_fail(exec_id, step, attempt, &error).await;
                if let Some(exec) = self.executions.get_mut(exec_id) {
                    exec.phase = ExecPhase::NeedsFailure { step: step.to_string(), error };
                }
            }
            None => {} // Not an action step — shouldn't be called
        }
    }

    /// Dispatch all Ready(runner step) executions to idle runners.
    async fn dispatch_ready_steps(&mut self) {
        // Collect all executions that are Ready for a runner step
        let ready: Vec<(String, String, u32, String)> = self.executions.iter()
            .filter(|(_, e)| e.status == "running")
            .filter_map(|(id, e)| {
                if let ExecPhase::Ready { ref step, attempt } = e.phase {
                    if !self.is_action_step(id, step) {
                        Some((id.clone(), step.clone(), attempt, e.task_id.clone()))
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .collect();

        for (exec_id, step, attempt, task_id) in ready {
            // Guard: don't dispatch if this execution already has a step in-flight
            if self.has_active_runner(&exec_id) {
                continue;
            }

            let runner_id = match self.find_idle_runner() {
                Some(r) => r,
                None => break, // No more idle runners
            };

            // Look up runtime and workspace from workflow
            let (runtime, workspace) = {
                let exec = self.executions.get(&exec_id);
                let workflow_name = exec.map(|e| e.workflow.as_str());
                workflow_name
                    .and_then(|wf| self.workflows.get(wf))
                    .and_then(|engine| engine.steps.get(&step))
                    .map(|step_def| {
                        let runtime = step_def
                            .runtime
                            .as_ref()
                            .map(|r| serde_json::to_value(r).unwrap_or_default())
                            .unwrap_or_default();
                        let workspace = step_def
                            .workspace
                            .as_ref()
                            .map(|w| serde_json::to_value(w).unwrap_or_default())
                            .unwrap_or_default();
                        (runtime, workspace)
                    })
                    .unwrap_or_else(|| (serde_json::json!({}), serde_json::json!({})))
            };

            tracing::info!(
                exec = %exec_id,
                step = %step,
                attempt,
                runner = %runner_id,
                "dispatching step"
            );

            // Mark runner as busy immediately
            if let Some(runner) = self.runners.get_mut(&runner_id.0) {
                runner.idle = false;
                runner.current_execution = Some(exec_id.clone());
                runner.current_step = Some(step.clone());
            }

            match self
                .client
                .dispatch_step(&ox_core::client::DispatchStepParams {
                    execution_id: exec_id.clone(),
                    step: step.clone(),
                    runner_id: runner_id.clone(),
                    attempt,
                    task_id,
                    runtime,
                    workspace,
                })
                .await
            {
                Ok(_) => {
                    // Set AwaitingStep immediately so the scheduler doesn't
                    // try to dispatch again before step.dispatched arrives via SSE.
                    if let Some(exec) = self.executions.get_mut(&exec_id) {
                        exec.phase = ExecPhase::AwaitingStep;
                    }
                }
                Err(e) => {
                    tracing::error!(err = %e, "failed to dispatch step");
                    // Restore runner to idle
                    if let Some(runner) = self.runners.get_mut(&runner_id.0) {
                        runner.idle = true;
                        runner.current_execution = None;
                        runner.current_step = None;
                    }
                }
            }
        }
    }

    // ── Triggers ───────────────────────────────────────────────────

    async fn evaluate_pending_triggers(&mut self) {
        let triggers = std::mem::take(&mut self.pending_triggers);

        for trigger in triggers {
            self.evaluate_triggers_for_node(&trigger.node_id, &trigger.event_type, &trigger.tags)
                .await;
        }
    }

    async fn evaluate_triggers_for_node(
        &mut self,
        node_id: &str,
        event_type: &str,
        tags: &[String],
    ) {
        for engine in self.workflows.values() {
            for trigger in &engine.triggers {
                if trigger.on != event_type {
                    continue;
                }

                if let Some(ref tag_pattern) = trigger.tag
                    && !tags.iter().any(|t| t == tag_pattern) {
                        continue;
                    }

                // Dedup: skip if there's already an active execution
                let dominated = self.executions.values().any(|e| {
                    e.task_id == node_id
                        && e.workflow == trigger.workflow
                        && (e.status == "running"
                            || e.status == "completed"
                            || e.status == "escalated")
                });
                if dominated {
                    tracing::debug!(
                        node = %node_id,
                        workflow = %trigger.workflow,
                        "trigger suppressed: execution already exists"
                    );
                    continue;
                }

                // Check current cx state
                let cx_state = get_cx_node_state(&self.server_url, node_id).await;
                if let Some(ref state) = cx_state
                    && (state == "integrated" || state == "shadowed") {
                        tracing::debug!(
                            node = %node_id,
                            state = %state,
                            "trigger suppressed: node is {state}"
                        );
                        continue;
                    }

                tracing::info!(
                    node = %node_id,
                    workflow = %trigger.workflow,
                    "trigger matched, creating execution"
                );

                match self
                    .client
                    .create_execution(node_id, &trigger.workflow, &trigger.on)
                    .await
                {
                    Ok(exec_id) => {
                        tracing::info!(exec = %exec_id, "execution created by trigger");
                    }
                    Err(e) => {
                        tracing::error!(err = %e, "failed to create execution from trigger");
                    }
                }
            }
        }
    }

    // ── Tick ────────────────────────────────────────────────────────

    async fn on_tick(&self) {
        // Pool monitoring: drain surplus runners
        if self.pool_target > 0 {
            let active_count = self.runners.len();
            if active_count > self.pool_target {
                let surplus = active_count - self.pool_target;
                let mut drained = 0;
                for runner in self.runners.values() {
                    if drained >= surplus {
                        break;
                    }
                    if runner.idle {
                        tracing::info!(runner = %runner.id, "draining surplus runner");
                        if let Err(e) = self.client.drain_runner(&runner.id).await {
                            tracing::warn!(err = %e, "failed to drain runner");
                        }
                        drained += 1;
                    }
                }
            }
        }
    }
}

/// Check the current state of a cx node via the server projection.
async fn get_cx_node_state(server_url: &str, node_id: &str) -> Option<String> {
    let url = format!("{server_url}/api/state/cx");
    let resp = reqwest::Client::new()
        .get(&url)
        .send()
        .await
        .ok()?;
    let cx_state: serde_json::Value = resp.json().await.ok()?;
    let nodes = cx_state.get("nodes")?.as_object()?;
    let node = nodes.get(node_id)?;
    node.get("state").and_then(|s| s.as_str()).map(String::from)
}
