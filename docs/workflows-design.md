# Workflow Engine Internals

This document covers the implementation of the workflow engine: trigger
evaluation, step graph traversal, transition matching, retry and visit
counting, escalation, the merge operation, and the interpolation engine.

For the workflow model (what workflows are), see
[prd/workflows.md](prd/workflows.md). For the execution lifecycle, see
[prd/execution.md](prd/execution.md).

---

## Trigger Evaluation

Triggers create executions. The herder evaluates triggers when source
events arrive on the SSE stream.

### Flow

```
EventType::Source arrives (SourceEventData { source, kind, subject_id, data })
  │
  ▼
herder: for each trigger in loaded trigger files:
  │
  ├─ does trigger.on match the event's `kind`?
  ├─ if trigger.source is set, does it match the event's `source`?
  ├─ does every [trigger.where] predicate match the event context?
  │
  ├─ build EventContext::Source from the envelope
  │  (source, kind, subject_id, data — all available as
  │   {event.source}, {event.kind}, {event.subject_id},
  │   {event.data.*})
  │
  ├─ call trigger.build_vars(&ctx):
  │  ├─ interpolate each [trigger.vars] template
  │  │  against {event.X} / {event.data.X} fields
  │  ├─ on MissingEventField → post trigger.failed, continue
  │  └─ return the workflow vars map
  │
  ├─ dedup check: is_origin_active(
  │     existing executions,
  │     origin = Source { source, kind, subject_id },
  │     workflow = trigger.workflow,
  │     is_active = |s| running or escalated
  │  )?
  │
  ├─ if all pass:
  │    POST /api/executions
  │    { workflow, vars, origin: Source{..}, trigger: <kind> }
  │
  └─ if dedup blocks or build_vars fails: skip (event emitted on failure)
```

Source-side state suppression (e.g. skip-if-`integrated` for cx nodes)
is **not** performed by the herder. Each watcher filters its own
source: a cx watcher does not POST a
`node.ready` event for a node that is already `integrated` or
`shadowed`. The server-side matcher has no special-cased knowledge of
any source's lifecycle — it matches what the watcher chose to ingest.

### Dedup Rules

A trigger is suppressed if there is already an active execution with
the same `(origin, workflow)` pair, where `origin` is typed
(`ExecutionOrigin::Source | Execution | Manual`) and compared
structurally. For source events the dedup key is
`(source, kind, subject_id)` — the same subject firing the same kind
of event does not double-start a workflow while one is already live.

The herder's `is_origin_active` predicate uses the running-or-escalated
liveness rule:

- **Herder auto-evaluation**: blocks on `running | escalated`. The
  herder does not auto-retry an escalated execution — that is a
  human-in-the-loop decision.

Manual re-runs after intervention are done by mutating the source
system (re-tagging a cx node, adding a Linear comment) or by posting
a synthetic event directly to `/api/events/ingest`. There is no
dedicated `ox-ctl trigger` command.

Dedup state is derived from the executions projection — no separate
tracking needed.

### Trigger Failures

When a trigger matches an event but cannot produce a valid execution,
the failure is recorded in the event log as `trigger.failed` rather
than silently dropped. This happens in three cases:

1. `trigger.build_vars` returns `MissingEventField { path }` — the
   `[trigger.vars]` block references an `event.*` field not exposed
   by the firing event type.
2. `WorkflowDef::validate_vars` rejects the interpolated vars map
   (e.g. a required var is not mapped) — `ValidationFailed { message }`.
3. The trigger's `workflow` is not loaded in the current config —
   `UnknownWorkflow`.

The herder posts its own failures through the `/api/triggers/failed`
endpoint so all `trigger.failed` events flow through the server's
event bus. Emission is guarded by `!replaying` so a bad trigger in
the config does not re-emit on every restart.

## Step Graph Traversal

A workflow's steps form a linear sequence with conditional jumps.
It is not a DAG — steps are ordered by declaration, and transitions
can jump forward or backward.

### Data Structure

```rust
pub struct WorkflowEngine {
    /// Steps indexed by name for O(1) lookup.
    steps: IndexMap<String, StepDef>,
}
```

`IndexMap` preserves insertion order (declaration order) while
providing O(1) name lookup. This is important because the default
"next step" is the one following the current step in declaration order.

### Advancing

After a step is confirmed, the herder determines the next step:

```rust
fn next_step(
    workflow: &WorkflowEngine,
    current_step: &str,
    output: &str,
    visit_counts: &mut HashMap<String, u32>,
) -> StepAdvance {
    // 1. Check transitions on the current step
    let step_def = &workflow.steps[current_step];

    for transition in &step_def.transitions {
        if transition_matches(&transition.match_pattern, output) {
            let target = &transition.goto;
            if target == "complete" {
                return StepAdvance::Complete;
            }
            if target == "escalate" {
                return StepAdvance::Escalate;
            }
            return check_visits(workflow, target, visit_counts);
        }
    }

    // 2. No transition matched — advance to next in declaration order
    let current_idx = workflow.steps.get_index_of(current_step).unwrap();
    match workflow.steps.get_index(current_idx + 1) {
        Some((name, _)) => check_visits(workflow, name, visit_counts),
        None => StepAdvance::Complete, // no more steps
    }
}

fn check_visits(
    workflow: &WorkflowEngine,
    target: &str,
    visit_counts: &mut HashMap<String, u32>,
) -> StepAdvance {
    let count = visit_counts.entry(target.to_string()).or_insert(0);
    *count += 1;

    let step_def = &workflow.steps[target];
    if let Some(max) = step_def.max_visits {
        if *count > max {
            let goto = step_def.max_visits_goto
                .as_deref()
                .unwrap_or("escalate");
            if goto == "complete" {
                return StepAdvance::Complete;
            }
            if goto == "escalate" {
                return StepAdvance::Escalate;
            }
            return StepAdvance::Goto(goto.to_string());
        }
    }

    StepAdvance::Goto(target.to_string())
}

enum StepAdvance {
    Goto(String),
    Escalate,
    Complete,
}
```

### Visit Counts

Visit counts are tracked per execution in `ExecutionState::visit_counts`.
They persist across the full execution — a step visited twice due to a
review loop has `visit_count = 2` regardless of how many other steps
ran between visits.

Visit counts are incremented when a step is dispatched, not when it
completes. This ensures that a step that fails and retries still counts
each attempt against the visit limit.

---

## Transition Matching

Transitions match on the step's output value. The match is a prefix
match — the output `"pass:7"` matches the pattern `"pass"`.

```rust
fn transition_matches(pattern: &str, output: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    output == pattern || output.starts_with(&format!("{pattern}:"))
}
```

The `:` delimiter allows outputs to carry structured data. A reviewer
can output `pass:7` (pass with confidence 7) — the transition matches
on `pass`, and the full value `pass:7` is available as `{prev_output}`
in the next step.

Transitions are evaluated in declaration order. The first match wins.
A `*` pattern is a catch-all that should appear last.

---

## Retry State Machine

Each step has a retry budget. The default is 3 attempts, overridable
per step with `max_retries`.

```
step fails
  │
  ▼
check retry budget
  │
  ├─ retries remaining:
  │    emit step.retrying
  │    re-dispatch same step with attempt += 1
  │
  └─ retries exhausted:
       ├─ on_fail = step name → jump to that step
       ├─ on_fail = "escalate" → escalate
       └─ on_fail absent → escalate
```

### State Tracking

Retry count is tracked per step per execution. It resets when the
execution visits a different step — if a review loop sends execution
back to `propose`, the retry count for `propose` is reset to 0.

```rust
pub struct RetryTracker {
    /// Current retry counts per step name.
    /// Reset when a different step is dispatched.
    counts: HashMap<String, u32>,
    last_step: Option<String>,
}

impl RetryTracker {
    fn record_failure(&mut self, step: &str, max_retries: u32) -> RetryDecision {
        // Reset if we moved to a different step
        if self.last_step.as_deref() != Some(step) {
            self.counts.clear();
            self.last_step = Some(step.to_string());
        }

        let count = self.counts.entry(step.to_string()).or_insert(0);
        *count += 1;

        if *count <= max_retries {
            RetryDecision::Retry { attempt: *count + 1 }
        } else {
            RetryDecision::Exhausted
        }
    }
}
```

Note: retry count is distinct from visit count. Retries are consecutive
failures of the same step. Visits are total dispatches of a step across
the full execution, including successful visits in review loops.

---

## Escalation

When retries exhaust and `on_fail` is absent or `"escalate"`:

1. The herder shadows the cx task:
   - Sets the node state to a "shadowed" marker (via cx `meta` field
     update on main, written by ox-server)
   - Shadowed nodes are skipped by trigger evaluation — they do not
     fire new executions
2. Emits `execution.escalated` with the step name and reason
3. If the workflow defines an escalation step (a step named `escalate`
   or referenced by `max_visits_goto`), that step is dispatched
4. If no escalation step exists, the execution terminates

### Shadowed Nodes

Shadowing is reversible. A human can un-shadow a node by updating its
`meta` field (via an interactive step or direct cx manipulation on a
branch that merges to main). The herder checks the shadow flag in
`meta` during trigger evaluation.

```json
{
  "meta": {
    "shadowed": true,
    "shadow_reason": "retries exhausted at step review-code",
    "shadow_execution": "aJuO-e1"
  }
}
```

---

## merge_to_main

The merge step runs in-process on ox-server via the herder's scheduler.
It is the only path for code and cx state to land on main.

### Implementation

Uses git CLI. **Main never leaves HEAD** — no branch checkouts,
no rebase. All operations happen on main.

```rust
fn merge_to_main(repo_path: &Path, branch: &str, squash: bool)
    -> Result<MergeResult, MergeError>
{
    // Preconditions: on main, clean worktree, branch exists + has commits

    let ahead = count_commits_ahead("main", branch);

    match ahead {
        // 1 commit → fast-forward (preserves agent's commit)
        1 => git(&["merge", "--ff-only", branch])?,

        // >1 + squash → squash merge on main
        n if squash => {
            let messages = git(&["log", "--reverse", "--format=%B",
                                 &format!("main..{branch}")])?;
            git(&["merge", "--squash", branch])?;
            git(&["commit", "-m", &messages])?;
        }

        // >1 + no squash → merge commit
        _ => git(&["merge", "--no-ff", "--no-edit", branch])?,
    }
}
```

The `squash` flag is set per step in the workflow definition. When
enabled and the branch has >1 commit, `git merge --squash` stages all
changes on main and commits with concatenated messages. If the agent
already squashed (1 commit ahead), fast-forward preserves it as-is.

On conflict in any path, the merge is aborted and the step fails.
The repo is always left clean on main.

### Post-Merge: Source Side Effects

After a successful merge, ox-server emits `git.merged` and returns.
It does **not** diff `.complex/` or derive any source-specific
events itself — that is a watcher's job.

Each source watcher observes its own surface independently. When a
merge to main changes `.complex/`, `ox-cx-watcher` sees the new
commits on its next tick, runs `cx log --since <cursor>` and
`cx show` for each touched node, maps them into
`EventType::Source` envelopes, and posts them to
`/api/events/ingest`. A Linear or GitHub watcher performs the same
role through its own API.

The result is that there is exactly one code path for source events
— the watcher ingest path — regardless of whether the change came
from a step in the workflow, a merge operation, or a human editing
files directly. See [prd/cx.md](prd/cx.md) and
[prd/event-sources.md](prd/event-sources.md).

---

## Interpolation Engine

The interpolation engine resolves `{name}` placeholders in strings.
It is used in runtime command templates, environment variables, file
mappings, and prompt fields.

### Implementation

```rust
pub struct InterpolationContext {
    values: HashMap<String, String>,
    secrets: HashMap<String, String>,
}

impl InterpolationContext {
    /// Create a context from built-in variables and runtime fields.
    pub fn new(
        builtins: &HashMap<String, String>,
        fields: &HashMap<String, toml::Value>,
        field_defs: &IndexMap<String, FieldDef>,
    ) -> Self {
        let mut values = builtins.clone();

        for (name, def) in field_defs {
            if let Some(value) = fields.get(name) {
                let resolved = match def.field_type {
                    FieldType::String => value.as_str().unwrap().to_string(),
                    FieldType::File => resolve_file_path(value.as_str().unwrap()),
                    FieldType::Bool => value.as_bool().unwrap().to_string(),
                    FieldType::Int => value.as_integer().unwrap().to_string(),
                };
                values.insert(name.clone(), resolved);
            } else if let Some(default) = &def.default {
                values.insert(name.clone(), default.clone());
            }
            // Absent optional fields: not inserted → interpolation
            // handles them specially per context (see below)
        }

        Self { values }
    }

    /// Interpolate a string. Returns None if a required field is absent.
    /// Handles both `{name}` (field/builtin) and `{secret.name}` (secret) references.
    pub fn interpolate(&self, template: &str) -> Result<String, InterpolationError> {
        let mut result = String::with_capacity(template.len());
        let mut chars = template.chars().peekable();

        while let Some(ch) = chars.next() {
            if ch == '{' {
                let name: String = chars.by_ref().take_while(|&c| c != '}').collect();
                if let Some(secret_name) = name.strip_prefix("secret:") {
                    match self.secrets.get(secret_name) {
                        Some(value) => result.push_str(value),
                        None => return Err(InterpolationError::MissingSecret(
                            secret_name.to_string(),
                        )),
                    }
                } else {
                    match self.values.get(&name) {
                        Some(value) => result.push_str(value),
                        None => return Err(InterpolationError::MissingField(name)),
                    }
                }
            } else {
                result.push(ch);
            }
        }

        Ok(result)
    }

    /// Collect all `{secret.NAME}` references from a template without resolving them.
    /// Used by ox-server at dispatch time to determine which secrets a step needs.
    pub fn collect_secret_refs(template: &str) -> Vec<String> {
        let mut refs = vec![];
        let mut chars = template.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch == '{' {
                let name: String = chars.by_ref().take_while(|&c| c != '}').collect();
                if let Some(secret_name) = name.strip_prefix("secret:") {
                    refs.push(secret_name.to_string());
                }
            }
        }
        refs
    }

    /// Check if a field has a value (for `optional` blocks).
    pub fn has_field(&self, name: &str) -> bool {
        self.values.contains_key(name)
    }
}
```

### Absent Field Handling

The interpolation engine does not silently substitute empty strings for
absent fields. Instead, the caller decides:

- **`cmd`** — absent fields in the base command are an error (the
  command would be malformed)
- **`optional`** — the `when` field is checked with `has_field()`;
  if absent, the entire args block is skipped
- **`files`** — if `from` references an absent field, the file mapping
  is skipped
- **`env`** — if any field in the value is absent, the variable is
  not set

This is implemented at the call site, not in the interpolation engine
itself.

### Command Building

```rust
fn build_command(
    def: &RuntimeDef,
    ctx: &InterpolationContext,
    tty: bool,
) -> Result<Vec<String>> {
    // Select base command
    let base = if tty && def.command.interactive_cmd.is_some() {
        def.command.interactive_cmd.as_ref().unwrap()
    } else {
        &def.command.cmd
    };

    // Interpolate base args
    let mut args: Vec<String> = base.iter()
        .map(|arg| ctx.interpolate(arg))
        .collect::<Result<_, _>>()?;

    // Append optional args
    for opt in &def.command.optional {
        if ctx.has_field(&opt.when) {
            for arg in &opt.args {
                args.push(ctx.interpolate(arg)?);
            }
        }
    }

    Ok(args)
}
```

---

## Herder Scheduling Loop

The herder separates state updates from decision-making. Event handlers
are pure projections — they update local state and nothing else. A
single `schedule()` function runs after each event and is the only
source of side-effects (API calls, dispatching, action execution).

### Execution Scheduler State

Each execution tracks scheduler state that tells the herder what action
(if any) is needed:

```rust
enum ExecPhase {
    /// Step is in-flight on a runner — nothing to do.
    AwaitingStep,
    /// Step confirmed — scheduler should advance the workflow.
    NeedsAdvance { step: String },
    /// Step failed — scheduler should retry or escalate.
    NeedsFailure { step: String, error: String },
    /// Next step determined — needs dispatching to a runner
    /// (or inline execution if it's an action step).
    Ready { step: String, attempt: u32 },
    /// Terminal — completed, escalated, or cancelled.
    Done,
}
```

Event handlers set scheduler state:

| Event | Scheduler state transition |
|-------|-----------------|
| `execution.created` | → `Ready { first_step, 1 }` |
| `step.dispatched` | → `AwaitingStep` |
| `step.confirmed` | → `NeedsAdvance { step }` |
| `step.failed` | → `NeedsFailure { step, error }` |
| `step.timeout` | → `Ready { step, attempt }` (re-dispatch, not a workflow failure) |
| `runner.heartbeat_missed` | → `Ready { step, attempt }` (re-dispatch, not a workflow failure) |
| `execution.completed` | → `Done` |
| `execution.escalated` | → `Done` |
| `execution.cancelled` | → `Done` |

During replay, the last event for each execution determines its scheduler state.
After replay, the scheduler runs once and picks up any execution that
was mid-transition when the herder last stopped.

### The schedule() Function

```rust
async fn schedule(&mut self) {
    // Pass 1: Evaluate pending triggers
    self.evaluate_pending_triggers().await;

    // Pass 2: Process execution state machines (loop until stable)
    loop {
        let mut changed = false;

        let exec_ids: Vec<String> = self.executions.keys()
            .filter(|id| self.executions[*id].status == "running")
            .cloned()
            .collect();

        for exec_id in exec_ids {
            match self.process_execution(&exec_id).await {
                PhaseResult::Changed => changed = true,
                PhaseResult::Unchanged => {}
            }
        }

        if !changed { break; }
    }

    // Pass 3: Dispatch - match Ready(runner step) to idle runners
    self.dispatch_ready_steps().await;
}
```

`process_execution` handles a single execution's scheduler state:

- **NeedsAdvance**: calls `engine.next_step()` to determine the next
  step via transition matching and visit counting. Sets `Ready` on
  success, `Done` on completion/escalation. Emits `step.advanced`.

- **NeedsFailure**: calls `retry_tracker.record_failure()`. On retry,
  sets `Ready { same_step, next_attempt }`. On exhaustion, checks
  `on_fail` and `max_visits` on the target — sets `Ready` for the
  on_fail target or `Done` for escalation.

- **Ready (action step)**: executes the action inline (e.g. calls
  `merge_to_main`). On success, stores output and sets `NeedsAdvance`.
  On failure, sets `NeedsFailure`. Emits step events for the action.

- **Ready (runner step)**: skipped here and handled in the dispatch pass.

- **AwaitingStep / Done**: no action needed.

The inner loop is necessary because action steps resolve synchronously.
A confirmed step may advance to an action step, which completes
immediately and needs another advance. The loop terminates because each
iteration either moves an execution to `AwaitingStep`/`Done` (stable)
or makes no progress (all remaining are `Ready` for runner steps or
already stable).

### Visit Count Tracking

Visit counts are incremented in exactly one place during live operation:
the `check_visits` function called from `process_execution` when
handling `NeedsAdvance` or the `on_fail` path in `NeedsFailure`. This
is the canonical source of truth.

During replay, visit counts are reconstructed from `step.dispatched`
events — one increment per dispatch. This matches the live behavior
where each visit goes through `check_visits` (increment) → `Ready` →
dispatch. Retries from the failure path also dispatch, so the replay
count matches.

There is no double-counting. Event handlers never touch visit counts
during live operation. The scheduler owns them.

### Dispatch

```rust
async fn dispatch_ready_steps(&mut self) {
    let ready: Vec<(String, String, u32)> = self.executions.iter()
        .filter(|(_, e)| matches!(&e.phase, ExecPhase::Ready { .. })
            && e.status == "running")
        .map(|(id, e)| match &e.phase {
            ExecPhase::Ready { step, attempt } =>
                (id.clone(), step.clone(), *attempt),
            _ => unreachable!(),
        })
        .collect();

    for (exec_id, step, attempt) in ready {
        if let Some(runner_id) = self.find_idle_runner() {
            self.dispatch_step(&exec_id, &step, &runner_id, attempt).await;
            // state transitions to AwaitingStep when step.dispatched
            // event arrives back through SSE
        }
    }
}
```

v1 uses simple first-idle runner selection. No affinity, no label
matching, no priority queues. There is no separate pending queue — the
`Ready` scheduler state on the execution serves that purpose. An execution in
`Ready` that cannot be dispatched (no idle runner) simply stays in
`Ready` until the next scheduling pass.
