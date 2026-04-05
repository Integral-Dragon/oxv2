# Workflow Engine Internals

This document covers the implementation of the workflow engine: trigger
evaluation, step graph traversal, transition matching, retry and visit
counting, escalation, the merge operation, and the interpolation engine.

For the workflow model (what workflows are), see
[prd/workflows.md](prd/workflows.md). For the execution lifecycle, see
[prd/execution.md](prd/execution.md).

---

## Trigger Evaluation

Triggers create executions. The herder evaluates triggers when cx events
arrive on the SSE stream.

### Flow

```
cx.task_ready event arrives
  │
  ▼
herder: for each trigger in all loaded workflows:
  │
  ├─ does trigger.on match the event type?
  ├─ does trigger.tag match any tag on the node?
  ├─ dedup check: is there already an active execution
  │  for this (task_id, workflow) pair?
  │
  ├─ if all pass:
  │    POST /api/executions
  │    { task_id, workflow, trigger: "cx.task_ready" }
  │
  └─ if dedup blocks: skip
```

### Dedup Rules

A trigger is suppressed if there is already an active execution
(status = `running`) for the same `(task_id, workflow)` pair. This
prevents duplicate executions when the same cx event is processed
multiple times (e.g. after herder restart replay).

The `--force` flag on `ox-ctl trigger` bypasses dedup. This is used
for manual re-runs after intervention.

Dedup state is derived from the executions projection — no separate
tracking needed. The herder checks `ExecutionsState` for an active
execution matching the task and workflow.

### Poll Triggers

Triggers with `poll_interval` fire repeatedly while their condition
holds. The herder tracks these in a `HashMap<TriggerId, Instant>` of
last-fired times. On each tick:

1. For each poll trigger, check if `poll_interval` has elapsed since
   last fire
2. If yes, evaluate the condition against current cx state
3. If condition holds and dedup passes, create an execution
4. Update last-fired time

When the condition becomes false (e.g. the node is no longer in the
triggering state), the trigger stops firing automatically — the
condition check fails.

---

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

The merge step runs in-process on ox-server. It is the only path for
code and cx state to land on main.

### Implementation

```rust
fn merge_to_main(repo: &Repository, branch: &str) -> Result<MergeResult> {
    let main_ref = repo.find_branch("main", BranchType::Local)?;
    let main_commit = main_ref.get().peel_to_commit()?;

    let branch_ref = repo.find_branch(branch, BranchType::Local)?;
    let branch_commit = branch_ref.get().peel_to_commit()?;

    // Precondition: worktree must be clean
    if !repo.statuses(None)?.is_empty() {
        return Err(MergeError::DirtyWorktree);
    }

    // Precondition: branch must have commits ahead of merge base
    let merge_base = repo.merge_base(main_commit.id(), branch_commit.id())?;
    if branch_commit.id() == merge_base {
        return Err(MergeError::EmptyBranch);
    }

    // Strategy 1: fast-forward
    if main_commit.id() == merge_base {
        // Branch is a descendant of main — fast-forward
        repo.reference(
            "refs/heads/main",
            branch_commit.id(),
            true,
            &format!("fast-forward merge of {branch}"),
        )?;
        return Ok(MergeResult::FastForward {
            prev_head: main_commit.id(),
            new_head: branch_commit.id(),
        });
    }

    // Strategy 2: merge commit
    let merge_base_commit = repo.find_commit(merge_base)?;
    let mut index = repo.merge_commits(&main_commit, &branch_commit, None)?;

    if index.has_conflicts() {
        return Err(MergeError::Conflicts { branch: branch.to_string() });
    }

    let tree_oid = index.write_tree_to(repo)?;
    let tree = repo.find_tree(tree_oid)?;
    let sig = repo.signature()?;

    let merge_commit = repo.commit(
        Some("refs/heads/main"),
        &sig,
        &sig,
        &format!("Merge branch '{branch}' into main"),
        &tree,
        &[&main_commit, &branch_commit],
    )?;

    Ok(MergeResult::MergeCommit {
        prev_head: main_commit.id(),
        new_head: merge_commit,
    })
}
```

### Post-Merge: cx Event Derivation

After a successful merge, ox-server diffs `.complex/` between the
previous and new main HEAD:

```rust
fn derive_cx_events(repo: &Repository, prev: Oid, new: Oid) -> Vec<CxEvent> {
    let prev_tree = repo.find_commit(prev).unwrap().tree().unwrap();
    let new_tree = repo.find_commit(new).unwrap().tree().unwrap();

    let diff = repo.diff_tree_to_tree(
        Some(&prev_tree),
        Some(&new_tree),
        Some(DiffOptions::new().pathspec(".complex/nodes/")),
    ).unwrap();

    let mut events = vec![];

    for delta in diff.deltas() {
        let path = delta.new_file().path().unwrap();
        let node_id = extract_node_id(path);

        match delta.status() {
            Delta::Added => {
                let node = parse_node(repo, delta.new_file());
                events.push(derive_creation_event(node_id, &node));
            }
            Delta::Modified => {
                let old_node = parse_node(repo, delta.old_file());
                let new_node = parse_node(repo, delta.new_file());
                events.extend(derive_modification_events(node_id, &old_node, &new_node));
            }
            _ => {}
        }
    }

    events
}

fn derive_modification_events(id: &str, old: &CxNode, new: &CxNode) -> Vec<CxEvent> {
    let mut events = vec![];

    // State transition
    if old.state != new.state {
        match new.state.as_str() {
            "ready" => events.push(CxEvent::TaskReady {
                node_id: id.to_string(),
                tags: new.tags.clone(),
            }),
            "claimed" => events.push(CxEvent::TaskClaimed {
                node_id: id.to_string(),
            }),
            "integrated" => events.push(CxEvent::TaskIntegrated {
                node_id: id.to_string(),
            }),
            _ => {}
        }
    }

    // New comments
    let old_comment_count = old.comments.len();
    for comment in &new.comments[old_comment_count..] {
        events.push(CxEvent::CommentAdded {
            node_id: id.to_string(),
            tag: comment.tag.clone(),
            author: comment.author.clone(),
        });
    }

    events
}
```

These derived cx events are appended to the ox event log immediately
after the `git.merged` event. They trigger further workflow actions
(e.g. `cx.task_integrated` may trigger a phase completion check).

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
    /// Handles both `{name}` (field/builtin) and `{secret:name}` (secret) references.
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

    /// Collect all `{secret:NAME}` references from a template without resolving them.
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

## Dispatch Decision

The herder's dispatch logic selects a runner for a pending step:

```rust
fn select_runner(pool: &PoolState) -> Option<RunnerId> {
    // Find first idle runner
    pool.runners.values()
        .find(|r| r.status == RunnerStatus::Idle)
        .map(|r| r.id.clone())
}
```

v1 uses simple first-idle selection. No affinity, no label matching,
no priority queues. When no idle runner is available, the step is
queued — the herder retries dispatch when a `step.confirmed` or
`runner.registered` event frees a runner.

### Pending Step Queue

Steps waiting for a runner are tracked in the herder's local state:

```rust
pub struct PendingQueue {
    /// Steps waiting for an idle runner, in FIFO order.
    queue: VecDeque<PendingStep>,
}

pub struct PendingStep {
    pub execution_id: ExecutionId,
    pub step: String,
    pub attempt: u32,
    pub dispatched_at: Option<Instant>,
}
```

When a runner becomes idle (via `step.confirmed`, `step.failed`, or
`runner.registered`), the herder pops the front of the queue and
dispatches.
