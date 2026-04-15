use anyhow::{Context, Result};
use futures_util::StreamExt;
use ox_core::client::{OxClient, OxClientApi};
use ox_core::events::*;
use ox_core::types::*;
use ox_core::workflow::{RetryDecision, RetryTracker, StepAdvance, TriggerDef, WorkflowDef, WorkflowEngine};
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
    vars: HashMap<String, String>,
    origin: ExecutionOrigin,
    workflow: String,
    status: String, // "running", "completed", "escalated", "cancelled"
    phase: ExecPhase,
    visit_counts: HashMap<String, u32>,
    last_output: Option<String>,
    retry_tracker: RetryTracker,
}

pub struct Herder<C: OxClientApi = OxClient> {
    client: C,
    server_url: String,
    pool_target: usize,
    #[allow(dead_code)]
    heartbeat_grace: Duration,
    tick_interval: Duration,

    // Local state rebuilt from SSE
    runners: HashMap<String, RunnerView>,
    executions: HashMap<String, ExecutionView>,
    workflows: HashMap<String, WorkflowEngine>,
    triggers: Vec<TriggerDef>,
    pending_source_events: Vec<SourceEventData>,
    last_seq: u64,
    /// True while replaying historical events — suppresses side effects.
    replaying: bool,
    /// The server's current seq at startup, used to detect end of replay.
    replay_target: u64,
    /// Track last-fired times for poll triggers.
    #[allow(dead_code)]
    poll_trigger_times: HashMap<(String, usize), Instant>,
    /// Last time we refreshed config from the server.
    last_config_refresh: Instant,
}

impl Herder<OxClient> {
    pub fn new(
        server_url: &str,
        pool_target: usize,
        heartbeat_grace_secs: u64,
        tick_interval_secs: u64,
    ) -> Self {
        Self::with_client(
            OxClient::new(server_url),
            server_url,
            pool_target,
            heartbeat_grace_secs,
            tick_interval_secs,
        )
    }
}

impl<C: OxClientApi> Herder<C> {
    /// Generic constructor — used by tests with mock client implementations.
    pub fn with_client(
        client: C,
        server_url: &str,
        pool_target: usize,
        heartbeat_grace_secs: u64,
        tick_interval_secs: u64,
    ) -> Self {
        Self {
            client,
            server_url: server_url.trim_end_matches('/').to_string(),
            pool_target,
            heartbeat_grace: Duration::from_secs(heartbeat_grace_secs),
            tick_interval: Duration::from_secs(tick_interval_secs),
            runners: HashMap::new(),
            executions: HashMap::new(),
            workflows: HashMap::new(),
            triggers: Vec::new(),
            pending_source_events: Vec::new(),
            last_seq: 0,
            replaying: true,
            replay_target: 0,
            poll_trigger_times: HashMap::new(),
            last_config_refresh: Instant::now(),
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

        // Load config and triggers
        let config = ox_core::config::load_config(&search_path);
        self.triggers = ox_core::config::load_triggers(&config);
        tracing::info!(count = self.triggers.len(), "loaded triggers from config");

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
                tracing::info!(exec = %d.execution_id, workflow = %d.workflow, "execution created");

                // Determine first step
                let first_step = self.workflows.get(&d.workflow)
                    .and_then(|e| e.first_step())
                    .map(|s| s.to_string())
                    .unwrap_or_default();

                // Events from pre-slice-5 logs may lack an origin;
                // fall back to Manual so the execution still lands in
                // local state.
                let origin = d
                    .origin
                    .clone()
                    .unwrap_or(ExecutionOrigin::Manual { user: None });

                self.executions.insert(
                    d.execution_id.0.clone(),
                    ExecutionView {
                        vars: d.vars,
                        origin,
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
                        // Infrastructure failure, not a workflow failure — re-dispatch
                        // without burning a retry.
                        tracing::warn!(exec = %d.execution_id, step = %d.step, timeout_secs = d.timeout_secs, "step timed out, re-dispatching");
                        exec.phase = ExecPhase::Ready { step: d.step, attempt: d.attempt };
                    }
            }
            "step.advanced" => {
                // Pure state — no action needed, scheduler already handled it
            }

            // Source events (from watcher plugins) — queue for matching.
            "source" => {
                let d: SourceEventData = serde_json::from_value(envelope.data)?;
                tracing::info!(
                    source = %d.source,
                    kind = %d.kind,
                    subject = %d.subject_id,
                    replaying = self.replaying,
                    "source event"
                );
                if !self.replaying {
                    self.pending_source_events.push(d);
                }
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


    // ── The Scheduler ──────────────────────────────────────────────

    async fn schedule(&mut self) {
        // Phase 1: Evaluate pending source-event triggers queued by
        // handle_event. Source events arrive on the SSE stream as
        // `EventType::Source` envelopes from watcher plugins.
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
                    let attempt = exec.visit_counts.get(&next_step).copied().unwrap_or(1);
                    exec.phase = ExecPhase::Ready { step: next_step, attempt };
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

                // Resolve branch from workspace spec + execution vars
                let branch = match self.executions.get(exec_id) {
                    Some(e) => {
                        let raw = step_def.workspace.as_ref()
                            .and_then(|w| w.branch.as_deref())
                            .unwrap_or("main")
                            .to_string();
                        let mut resolved = raw;
                        for (k, v) in &e.vars {
                            resolved = resolved.replace(&format!("{{{k}}}"), v);
                        }
                        resolved
                    }
                    None => return,
                };

                match self.client.merge_to_main(exec_id, step, &branch, step_def.squash).await {
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
        let ready: Vec<(String, String, u32, HashMap<String, String>)> = self.executions.iter()
            .filter(|(_, e)| e.status == "running")
            .filter_map(|(id, e)| {
                if let ExecPhase::Ready { ref step, attempt } = e.phase {
                    if !self.is_action_step(id, step) {
                        Some((id.clone(), step.clone(), attempt, e.vars.clone()))
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .collect();

        for (exec_id, step, attempt, vars) in ready {
            let runner_id = match self.find_idle_runner() {
                Some(r) => r,
                None => break, // No more idle runners
            };

            // Look up persona, prompt, runtime, and workspace from workflow
            let (persona, prompt, runtime, workspace) = {
                let exec = self.executions.get(&exec_id);
                let workflow_name = exec.map(|e| e.workflow.as_str());
                let wf_and_step = workflow_name
                    .and_then(|wf| self.workflows.get(wf).map(|engine| (wf, engine)));

                if let Some((_wf_name, engine)) = &wf_and_step {
                    if let Some(step_def) = engine.steps.get(&step) {
                        // Resolve persona: step-level overrides workflow-level
                        let persona = step_def.persona.clone()
                            .or_else(|| engine.persona.clone());
                        let prompt = step_def.prompt.clone();
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
                        (persona, prompt, runtime, workspace)
                    } else {
                        (None, None, serde_json::json!({}), serde_json::json!({}))
                    }
                } else {
                    (None, None, serde_json::json!({}), serde_json::json!({}))
                }
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
                    vars,
                    persona,
                    prompt,
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
        // Re-fetch config from server if stale (>30s since last refresh).
        // This picks up hot-reloaded workflows, triggers, and personas.
        if self.last_config_refresh.elapsed() > Duration::from_secs(30) {
            if let Err(e) = self.load_workflows().await {
                tracing::warn!(err = %e, "failed to refresh config from server");
            }
            self.last_config_refresh = Instant::now();
        }

        let source_events = std::mem::take(&mut self.pending_source_events);
        for event in source_events {
            self.evaluate_triggers_for_source_event(&event).await;
        }
    }

    /// Match source events (from watcher plugins) against configured
    /// triggers. A trigger matches when its `on` equals the event kind,
    /// its optional `source` equals the event source, and every
    /// `[trigger.where]` predicate matches the event context. Dedup is by
    /// `ExecutionOrigin::Source { source, kind, subject_id }`; vars
    /// template against an `EventContext::Source` context.
    ///
    /// Source-side state suppression (skip-if-integrated for cx, etc.)
    /// is the watcher's responsibility under the event-sources plan —
    /// the server-side matcher has no special-cased knowledge of any
    /// source's lifecycle.
    async fn evaluate_triggers_for_source_event(&mut self, event: &SourceEventData) {
        for trigger in &self.triggers {
            if trigger.on != event.kind {
                continue;
            }
            if let Some(ref want_source) = trigger.source
                && want_source != &event.source
            {
                continue;
            }
            let origin = ExecutionOrigin::Source {
                source: event.source.clone(),
                kind: event.kind.clone(),
                subject_id: event.subject_id.clone(),
            };

            // Dedup — herder blocks on running OR escalated
            // executions for the same (origin, workflow) pair.
            let existing: Vec<_> = self
                .executions
                .values()
                .map(|e| (&e.origin, e.workflow.as_str(), e.status.as_str()))
                .collect();
            if is_origin_active(
                existing.into_iter(),
                &origin,
                &trigger.workflow,
                |s| s == "running" || s == "escalated",
            ) {
                tracing::debug!(
                    source = %event.source,
                    kind = %event.kind,
                    subject = %event.subject_id,
                    workflow = %trigger.workflow,
                    "trigger suppressed: execution already exists"
                );
                continue;
            }

            // Build workflow vars from the `[trigger.vars]` block
            // resolved against an EventContext::Source. On a missing
            // field, emit trigger.failed and skip.
            let ctx = EventContext::Source {
                source: event.source.clone(),
                kind: event.kind.clone(),
                subject_id: event.subject_id.clone(),
                data: event.data.clone(),
            };
            if !trigger.matches_where(&ctx) {
                continue;
            }
            let trigger_vars = match trigger.build_vars(&ctx) {
                Ok(v) => v,
                Err(ox_core::workflow::TriggerError::MissingEventField { path }) => {
                    tracing::warn!(
                        source = %event.source,
                        kind = %event.kind,
                        subject = %event.subject_id,
                        workflow = %trigger.workflow,
                        path = %path,
                        "trigger var interpolation failed — missing event field"
                    );
                    let failed = TriggerFailedData::from_missing_field(
                        Seq(self.last_seq),
                        &trigger.on,
                        &trigger.workflow,
                        path,
                    );
                    if let Err(e) = self.client.post_trigger_failed(&failed).await {
                        tracing::warn!(err = %e, "failed to post trigger.failed event");
                    }
                    continue;
                }
            };

            tracing::info!(
                source = %event.source,
                kind = %event.kind,
                subject = %event.subject_id,
                workflow = %trigger.workflow,
                "trigger matched, creating execution"
            );

            let vars_for_local = trigger_vars.clone();
            let workflow_for_local = trigger.workflow.clone();
            match self
                .client
                .create_execution(
                    &trigger.workflow,
                    &trigger.on,
                    trigger_vars,
                    Some(origin.clone()),
                )
                .await
            {
                Ok(exec_id) => {
                    tracing::info!(exec = %exec_id, "execution created from source event");

                    // Optimistic local insert for dedup: a second
                    // event arriving before the SSE round-trip carrying
                    // execution.created back must see this execution in
                    // `self.executions` and skip the matcher.
                    self.executions.insert(
                        exec_id.0.clone(),
                        ExecutionView {
                            vars: vars_for_local,
                            origin: origin.clone(),
                            workflow: workflow_for_local,
                            status: "running".into(),
                            phase: ExecPhase::AwaitingStep,
                            visit_counts: HashMap::new(),
                            last_output: None,
                            retry_tracker: RetryTracker::new(),
                        },
                    );
                }
                Err(e) => {
                    tracing::error!(err = %e, "failed to create execution from source event");
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

#[cfg(test)]
mod tests {
    use super::*;
    use ox_core::client::{
        DispatchStepParams, OxClientApi, StatusResponse, WorkflowEntry,
    };
    use std::sync::Mutex;

    /// Mock implementing only the methods the SSE-driven trigger path
    /// actually uses. Unused trait methods panic.
    #[derive(Default)]
    struct MockClient {
        created: Mutex<Vec<(String, String, ExecutionOrigin)>>,
        created_vars: Mutex<Vec<HashMap<String, String>>>,
    }

    impl MockClient {
        fn new() -> Self {
            Self::default()
        }
        fn create_calls(&self) -> Vec<(String, String, ExecutionOrigin)> {
            self.created.lock().unwrap().clone()
        }
        fn create_call_vars(&self) -> Vec<HashMap<String, String>> {
            self.created_vars.lock().unwrap().clone()
        }
    }

    impl OxClientApi for MockClient {
        async fn status(&self) -> Result<StatusResponse> {
            unimplemented!("MockClient::status not used")
        }
        async fn list_workflows(&self) -> Result<Vec<WorkflowEntry>> {
            unimplemented!("MockClient::list_workflows not used")
        }
        async fn create_execution(
            &self,
            workflow: &str,
            trigger: &str,
            vars: HashMap<String, String>,
            origin: Option<ExecutionOrigin>,
        ) -> Result<ExecutionId> {
            self.created.lock().unwrap().push((
                workflow.to_string(),
                trigger.to_string(),
                origin.unwrap_or(ExecutionOrigin::Manual { user: None }),
            ));
            self.created_vars.lock().unwrap().push(vars);
            Ok(ExecutionId(format!("exec-mock-{}", self.created.lock().unwrap().len())))
        }
        async fn complete_execution(&self, _id: &str) -> Result<()> {
            unimplemented!()
        }
        async fn escalate_execution(&self, _id: &str, _step: &str, _reason: &str) -> Result<()> {
            unimplemented!()
        }
        async fn dispatch_step(&self, _params: &DispatchStepParams) -> Result<()> {
            unimplemented!()
        }
        async fn step_done(&self, _: &str, _: &str, _: u32, _: &str) -> Result<()> {
            unimplemented!()
        }
        async fn step_confirm(&self, _: &str, _: &str, _: u32, _: Option<serde_json::Value>) -> Result<()> {
            unimplemented!()
        }
        async fn step_fail(&self, _: &str, _: &str, _: u32, _: &str) -> Result<()> {
            unimplemented!()
        }
        async fn step_advance(&self, _: &str, _: &str, _: &str, _: &str) -> Result<()> {
            unimplemented!()
        }
        async fn drain_runner(&self, _: &RunnerId) -> Result<()> {
            unimplemented!()
        }
        async fn merge_to_main(&self, _: &str, _: &str, _: &str, _: bool) -> Result<serde_json::Value> {
            unimplemented!()
        }
        async fn post_trigger_failed(&self, _: &TriggerFailedData) -> Result<()> {
            unimplemented!()
        }
    }

    /// Source-event trigger that matches `EventType::Source` with
    /// `source = "cx"` and `kind = "node.ready"`.
    fn cx_source_trigger() -> TriggerDef {
        let mut vars = HashMap::new();
        vars.insert("branch".into(), "cx-{event.subject_id}".into());
        vars.insert("task_id".into(), "{event.subject_id}".into());
        vars.insert("title".into(), "{event.data.title}".into());
        let where_ = HashMap::from([(
            "data.tags".into(),
            ox_core::workflow::TriggerWhere::Contains {
                contains: "workflow:code-task".into(),
            },
        )]);
        TriggerDef {
            on: "node.ready".into(),
            source: Some("cx".into()),
            where_,
            workflow: "code-task".into(),
            poll_interval: None,
            vars,
        }
    }

    fn sample_cx_source_event() -> SourceEventData {
        SourceEventData {
            source: "cx".into(),
            kind: "node.ready".into(),
            subject_id: "Q6cY".into(),
            idempotency_key: "Q6cY:node.ready:sha-abc".into(),
            data: serde_json::json!({
                "title": "ccstat models — model-mix breakdown over time",
                "node_id": "Q6cY",
                "state": "ready",
                "tags": ["workflow:code-task"]
            }),
        }
    }

    // ── Source event triggers ───────────────────────────────────────

    fn herder_with_source_trigger(client: MockClient) -> Herder<MockClient> {
        let mut h = Herder::with_client(client, "http://test", 0, 60, 1);
        h.triggers = vec![cx_source_trigger()];
        h.replaying = false;
        h
    }

    /// A `source = "cx", on = "node.ready"` trigger fires when a
    /// matching source event arrives. The execution is created with
    /// `ExecutionOrigin::Source { source, kind, subject_id }` and the
    /// vars template resolves against `event.source`, `event.kind`,
    /// `event.subject_id`, and `event.data.*`.
    #[tokio::test]
    async fn source_event_fires_for_matching_source_and_kind() {
        let mut h = herder_with_source_trigger(MockClient::new());

        let event = sample_cx_source_event();
        h.evaluate_triggers_for_source_event(&event).await;

        let calls = h.client.create_calls();
        assert_eq!(calls.len(), 1, "one execution should be created");
        assert_eq!(calls[0].0, "code-task");
        assert_eq!(
            calls[0].2,
            ExecutionOrigin::Source {
                source: "cx".into(),
                kind: "node.ready".into(),
                subject_id: "Q6cY".into(),
            }
        );

        let vars = h.client.create_call_vars();
        assert_eq!(vars.len(), 1);
        assert_eq!(vars[0].get("branch").map(String::as_str), Some("cx-Q6cY"));
        assert_eq!(vars[0].get("task_id").map(String::as_str), Some("Q6cY"));
        assert_eq!(
            vars[0].get("title").map(String::as_str),
            Some("ccstat models — model-mix breakdown over time")
        );
    }

    /// A trigger with `source = "cx"` must NOT fire on an event from a
    /// different watcher. Source is a hard filter.
    #[tokio::test]
    async fn source_event_does_not_fire_for_different_source() {
        let mut h = herder_with_source_trigger(MockClient::new());

        let mut event = sample_cx_source_event();
        event.source = "linear".into();

        h.evaluate_triggers_for_source_event(&event).await;

        assert_eq!(h.client.create_calls().len(), 0);
    }

    /// A trigger with `on = "node.ready"` must NOT fire on a source
    /// event with a different kind — even when the source matches.
    #[tokio::test]
    async fn source_event_does_not_fire_for_different_kind() {
        let mut h = herder_with_source_trigger(MockClient::new());

        let mut event = sample_cx_source_event();
        event.kind = "node.claimed".into();

        h.evaluate_triggers_for_source_event(&event).await;

        assert_eq!(h.client.create_calls().len(), 0);
    }

    /// A trigger with a `[trigger.where]` filter must NOT fire when
    /// the event payload does not match it.
    #[tokio::test]
    async fn source_event_does_not_fire_when_where_missing() {
        let mut h = herder_with_source_trigger(MockClient::new());

        let mut event = sample_cx_source_event();
        event.data["tags"] = serde_json::json!(["workflow:other"]);

        h.evaluate_triggers_for_source_event(&event).await;

        assert_eq!(h.client.create_calls().len(), 0);
    }

    /// Dedup: a second source event with the same
    /// `(source, kind, subject_id)` tuple while the first execution is
    /// still live must NOT fire a second execution.
    #[tokio::test]
    async fn source_event_skipped_when_origin_already_active() {
        let mut h = herder_with_source_trigger(MockClient::new());

        let event = sample_cx_source_event();
        h.executions.insert(
            "exec-existing".into(),
            ExecutionView {
                vars: HashMap::new(),
                origin: ExecutionOrigin::Source {
                    source: "cx".into(),
                    kind: "node.ready".into(),
                    subject_id: "Q6cY".into(),
                },
                workflow: "code-task".into(),
                status: "running".into(),
                phase: ExecPhase::AwaitingStep,
                visit_counts: HashMap::new(),
                last_output: None,
                retry_tracker: RetryTracker::new(),
            },
        );

        h.evaluate_triggers_for_source_event(&event).await;

        assert_eq!(h.client.create_calls().len(), 0);
    }
}
