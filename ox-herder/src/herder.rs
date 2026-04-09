use anyhow::{Context, Result};
use futures_util::StreamExt;
use ox_core::client::OxClient;
use ox_core::events::*;
use ox_core::types::*;
use ox_core::workflow::{RetryDecision, RetryTracker, StepAdvance, WorkflowDef, WorkflowEngine};
use reqwest_eventsource::{Event as SseEvent, EventSource};
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

/// A step waiting to be dispatched to an idle runner.
#[derive(Debug)]
struct PendingStep {
    execution_id: String,
    task_id: String,
    step: String,
    attempt: u32,
    runtime: serde_json::Value,
    workspace: serde_json::Value,
}

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
#[allow(dead_code)]
struct ExecutionView {
    id: String,
    task_id: String,
    workflow: String,
    current_step: Option<String>,
    current_attempt: u32,
    status: String, // "running", "completed", "escalated", "cancelled"
    visit_counts: HashMap<String, u32>,
    last_output: Option<String>,
    retry_tracker: RetryTracker,
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
    pending: VecDeque<PendingStep>,
    last_seq: u64,
    /// Track last-fired times for poll triggers: (workflow_name, trigger_index) -> Instant
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
            pending: VecDeque::new(),
            last_seq: 0,
            poll_trigger_times: HashMap::new(),
        }
    }

    pub async fn run(&mut self) -> Result<()> {
        // Load workflow definitions from server
        self.load_workflows().await?;

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
                            backoff_secs = 1; // reset on successful connection
                        }
                        Ok(SseEvent::Message(msg)) => {
                            if let Err(e) = self.handle_sse_message(&msg.event, &msg.data).await {
                                tracing::warn!(err = %e, event = %msg.event, "error handling SSE event");
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
        // We need the full definitions to do transition matching.
        // For now, fetch them via the list endpoint. The herder needs step+transition info
        // which the list endpoint provides as names only. We need to enhance this.
        // For Phase 3, we load workflow TOMLs from the same search path the server uses.
        // TODO: Add a GET /api/workflows/{name} endpoint that returns the full definition.
        // For now, the herder relies on knowing workflow step order from the server's list.
        tracing::info!(count = workflows.len(), "loaded workflows from server");

        // We'll load workflow definitions from the config search path directly
        // since the herder needs the full step graph for transition matching.
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
                // Try to dispatch pending steps
                self.try_dispatch_pending().await;
            }
            "runner.drained" => {
                let d: RunnerDrainedData = serde_json::from_value(envelope.data)?;
                tracing::info!(runner = %d.runner_id, "runner drained");
                self.runners.remove(&d.runner_id.0);
            }

            "execution.created" => {
                let d: ExecutionCreatedData = serde_json::from_value(envelope.data)?;
                tracing::info!(exec = %d.execution_id, task = %d.task_id, workflow = %d.workflow, "execution created");
                self.executions.insert(
                    d.execution_id.0.clone(),
                    ExecutionView {
                        id: d.execution_id.0.clone(),
                        task_id: d.task_id,
                        workflow: d.workflow.clone(),
                        current_step: None,
                        current_attempt: 0,
                        status: "running".into(),
                        visit_counts: HashMap::new(),
                        last_output: None,
                        retry_tracker: RetryTracker::new(),
                    },
                );
                // Dispatch the first step
                self.dispatch_first_step(&d.execution_id.0, &d.workflow).await;
            }
            "execution.completed" => {
                let d: ExecutionCompletedData = serde_json::from_value(envelope.data)?;
                if let Some(exec) = self.executions.get_mut(&d.execution_id.0) {
                    exec.status = "completed".into();
                }
                tracing::info!(exec = %d.execution_id, "execution completed");
            }
            "execution.escalated" => {
                let d: ExecutionEscalatedData = serde_json::from_value(envelope.data)?;
                if let Some(exec) = self.executions.get_mut(&d.execution_id.0) {
                    exec.status = "escalated".into();
                }
                tracing::info!(exec = %d.execution_id, step = %d.step, reason = %d.reason, "execution escalated");
            }
            "execution.cancelled" => {
                let d: ExecutionCancelledData = serde_json::from_value(envelope.data)?;
                if let Some(exec) = self.executions.get_mut(&d.execution_id.0) {
                    exec.status = "cancelled".into();
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
                    exec.current_step = Some(d.step.clone());
                    exec.current_attempt = d.attempt;
                    *exec.visit_counts.entry(d.step.clone()).or_insert(0) += 1;
                }
            }
            "step.done" => {
                let d: StepDoneData = serde_json::from_value(envelope.data)?;
                // Pending — herder waits for step.confirmed
                if let Some(exec) = self.executions.get_mut(&d.execution_id.0) {
                    exec.last_output = Some(d.output);
                }
            }
            "step.confirmed" => {
                let d: StepConfirmedData = serde_json::from_value(envelope.data)?;
                tracing::info!(exec = %d.execution_id, step = %d.step, "step confirmed");

                // Free the runner
                self.free_runner_for_step(&d.execution_id.0, &d.step);

                // Reset retry tracker on success
                if let Some(exec) = self.executions.get_mut(&d.execution_id.0) {
                    exec.retry_tracker.reset();
                }

                // Advance the workflow
                self.advance_workflow(&d.execution_id.0, &d.step).await;

                // Try dispatch pending
                self.try_dispatch_pending().await;
            }
            "step.failed" => {
                let d: StepFailedData = serde_json::from_value(envelope.data)?;
                tracing::warn!(exec = %d.execution_id, step = %d.step, error = %d.error, "step failed");

                // Free the runner
                self.free_runner_for_step(&d.execution_id.0, &d.step);

                // Handle retry or escalate
                self.handle_step_failure(&d.execution_id.0, &d.step, &d.error).await;

                // Try dispatch pending
                self.try_dispatch_pending().await;
            }
            "step.advanced" => {
                // Already handled by our own advance call — just update local state
            }

            // cx events — evaluate triggers
            "cx.task_ready" => {
                let d: CxTaskReadyData = serde_json::from_value(envelope.data)?;
                tracing::info!(node = %d.node_id, tags = ?d.tags, "cx.task_ready");
                self.evaluate_triggers_for_node(&d.node_id, "cx.task_ready", &d.tags)
                    .await;
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

            _ => {
                // Other events (artifact, secret, etc.) — not handled by herder
            }
        }

        Ok(())
    }

    // ── Dispatch ────────────────────────────────────────────────────

    async fn dispatch_first_step(&mut self, execution_id: &str, workflow_name: &str) {
        let first_step = match self.workflows.get(workflow_name) {
            Some(engine) => match engine.first_step() {
                Some(s) => s.to_string(),
                None => {
                    tracing::error!(workflow = %workflow_name, "workflow has no steps");
                    return;
                }
            },
            None => {
                tracing::error!(workflow = %workflow_name, "unknown workflow");
                return;
            }
        };

        self.enqueue_step(execution_id, &first_step, 1).await;
    }

    async fn enqueue_step(&mut self, execution_id: &str, step: &str, attempt: u32) {
        // Check if this is an action step (merge_to_main) that doesn't need a runner
        if self.try_handle_action_step(execution_id, step).await {
            return;
        }

        // Look up execution info, then step def from workflow
        let exec_view = self.executions.get(execution_id);
        let task_id = exec_view.map(|e| e.task_id.clone()).unwrap_or_default();
        let workflow_name = exec_view.map(|e| e.workflow.clone());

        let (runtime, workspace) = workflow_name
            .as_deref()
            .and_then(|wf| self.workflows.get(wf))
            .and_then(|engine| engine.steps.get(step))
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
            .unwrap_or_else(|| (serde_json::json!({}), serde_json::json!({})));

        let pending = PendingStep {
            execution_id: execution_id.to_string(),
            task_id,
            step: step.to_string(),
            attempt,
            runtime,
            workspace,
        };

        self.pending.push_back(pending);
        self.try_dispatch_pending().await;
    }

    async fn try_dispatch_pending(&mut self) {
        while let Some(idle_runner_id) = self.find_idle_runner() {
            if let Some(pending) = self.pending.pop_front() {
                tracing::info!(
                    exec = %pending.execution_id,
                    step = %pending.step,
                    attempt = pending.attempt,
                    runner = %idle_runner_id,
                    "dispatching step"
                );

                if let Err(e) = self
                    .client
                    .dispatch_step(
                        &pending.execution_id,
                        &pending.step,
                        &idle_runner_id,
                        pending.attempt,
                        &pending.task_id,
                        pending.runtime,
                        pending.workspace,
                    )
                    .await
                {
                    tracing::error!(err = %e, "failed to dispatch step");
                    // Re-enqueue? For now just log.
                }
            } else {
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

    // ── Advance ─────────────────────────────────────────────────────

    async fn advance_workflow(&mut self, execution_id: &str, current_step: &str) {
        let (workflow_name, output, mut visit_counts) = {
            let exec = match self.executions.get(execution_id) {
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

        // Update visit counts back
        if let Some(exec) = self.executions.get_mut(execution_id) {
            exec.visit_counts = visit_counts;
        }

        match advance {
            StepAdvance::Goto(next_step) => {
                tracing::info!(exec = %execution_id, from = %current_step, to = %next_step, "advancing");
                // Emit advance event
                if let Err(e) = self
                    .client
                    .step_advance(execution_id, current_step, current_step, &next_step)
                    .await
                {
                    tracing::error!(err = %e, "failed to emit step.advanced");
                }
                // Enqueue the next step
                self.enqueue_step(execution_id, &next_step, 1).await;
            }
            StepAdvance::Complete => {
                tracing::info!(exec = %execution_id, "workflow complete");
                if let Err(e) = self.client.complete_execution(execution_id).await {
                    tracing::error!(err = %e, "failed to complete execution");
                }
                if let Some(exec) = self.executions.get_mut(execution_id) {
                    exec.status = "completed".into();
                }
            }
            StepAdvance::Escalate => {
                tracing::warn!(exec = %execution_id, step = %current_step, "escalating");
                if let Err(e) = self
                    .client
                    .escalate_execution(execution_id, current_step, "max visits exceeded or wildcard escalation")
                    .await
                {
                    tracing::error!(err = %e, "failed to escalate execution");
                }
                if let Some(exec) = self.executions.get_mut(execution_id) {
                    exec.status = "escalated".into();
                }
            }
        }
    }

    // ── Failure Handling ────────────────────────────────────────────

    async fn handle_step_failure(&mut self, execution_id: &str, step: &str, _error: &str) {
        let (max_retries, on_fail) = {
            let exec = match self.executions.get(execution_id) {
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
            let exec = match self.executions.get_mut(execution_id) {
                Some(e) => e,
                None => return,
            };
            exec.retry_tracker.record_failure(step, max_retries)
        };

        match decision {
            RetryDecision::Retry { attempt } => {
                tracing::info!(exec = %execution_id, step = %step, attempt, "retrying step");
                self.enqueue_step(execution_id, step, attempt).await;
            }
            RetryDecision::Exhausted => {
                match on_fail.as_deref() {
                    Some("escalate") | None => {
                        tracing::warn!(exec = %execution_id, step = %step, "retries exhausted, escalating");
                        if let Some(exec) = self.executions.get_mut(execution_id) {
                            exec.status = "escalated".into();
                        }
                    }
                    Some(goto_step) => {
                        tracing::info!(exec = %execution_id, from = %step, to = %goto_step, "retries exhausted, jumping to on_fail");
                        self.enqueue_step(execution_id, goto_step, 1).await;
                    }
                }
            }
        }
    }

    // ── Triggers ───────────────────────────────────────────────────

    async fn evaluate_triggers_for_node(
        &mut self,
        node_id: &str,
        event_type: &str,
        tags: &[String],
    ) {
        for (_workflow_name, engine) in &self.workflows {
            for trigger in &engine.triggers {
                // Check event type match
                if trigger.on != event_type {
                    continue;
                }

                // Check tag match
                if let Some(ref tag_pattern) = trigger.tag {
                    if !tags.iter().any(|t| t == tag_pattern) {
                        continue;
                    }
                }

                // Dedup check: is there already a running execution for this (task_id, workflow)?
                let already_running = self.executions.values().any(|e| {
                    e.task_id == node_id
                        && e.workflow == trigger.workflow
                        && e.status == "running"
                });
                if already_running {
                    tracing::debug!(
                        node = %node_id,
                        workflow = %trigger.workflow,
                        "trigger suppressed: active execution exists"
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

    // ── Action Steps ───────────────────────────────────────────────

    /// Check if a step has the `merge_to_main` action and handle it.
    /// Returns true if the step was an action step (no dispatch needed).
    async fn try_handle_action_step(
        &mut self,
        execution_id: &str,
        step: &str,
    ) -> bool {
        let workflow_name = match self.executions.get(execution_id) {
            Some(e) => e.workflow.clone(),
            None => return false,
        };

        let engine = match self.workflows.get(&workflow_name) {
            Some(e) => e,
            None => return false,
        };

        let step_def = match engine.steps.get(step) {
            Some(s) => s,
            None => return false,
        };

        match step_def.action.as_deref() {
            Some("merge_to_main") => {
                tracing::info!(exec = %execution_id, step = %step, "executing merge_to_main action");

                // Derive branch name from execution's task_id
                let task_id = match self.executions.get(execution_id) {
                    Some(e) => e.task_id.clone(),
                    None => return true,
                };

                // Request merge via server API
                match self
                    .client
                    .merge_to_main(execution_id, step, &task_id)
                    .await
                {
                    Ok(_) => {
                        tracing::info!(exec = %execution_id, "merge_to_main succeeded");
                        // Report done+confirm so workflow can advance
                        if let Err(e) = self
                            .client
                            .step_done(execution_id, step, 1, "pass")
                            .await
                        {
                            tracing::error!(err = %e, "failed to report merge done");
                        }
                        if let Err(e) = self
                            .client
                            .step_confirm(execution_id, step, 1, None)
                            .await
                        {
                            tracing::error!(err = %e, "failed to confirm merge step");
                        }
                    }
                    Err(e) => {
                        tracing::error!(err = %e, "merge_to_main failed");
                        if let Err(e2) = self
                            .client
                            .step_fail(execution_id, step, 1, &e.to_string())
                            .await
                        {
                            tracing::error!(err = %e2, "failed to report merge failure");
                        }
                    }
                }

                true
            }
            _ => false,
        }
    }

    // ── Tick ────────────────────────────────────────────────────────

    async fn on_tick(&self) {
        // Pool monitoring: drain surplus runners
        if self.pool_target > 0 {
            let active_count = self.runners.len();
            if active_count > self.pool_target {
                // Find idle runners to drain
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

        // TODO: heartbeat liveness checks (Phase 3 tick — check last_seen timestamps
        // from /api/state/pool and re-dispatch steps from stale runners)
    }
}
