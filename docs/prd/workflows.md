# Workflows

Workflows are the only way work gets done in Ox. A trigger creates a
workflow execution. The workflow engine dispatches steps to pool runners,
monitors completion via the event stream, and advances through the step
graph until the workflow completes or escalates.

No persistent AI sessions. No polling loops. No filesystem coordination.
The herder reacts to events; runners execute steps; the event log records
everything.

---

## Workflow Definitions

Workflows are TOML files found via the configuration search path (see
[README.md](README.md#configuration-search)) under `workflows/`. The
engine loads all definitions at startup. A workflow file contains a
`[workflow]` header and one or more `[[step]]` blocks.

```toml
[workflow]
name        = "code-task"
description = "Propose → review plan → implement → review code → merge"

[[step]]
name      = "propose"
workspace = { git_clone = true, branch = "{task_id}", push = true }
output    = "diff"
max_visits      = 3
max_visits_goto = "plan-tiebreak"

[step.runtime]
type    = "claude"
model   = "sonnet"
persona = "inspired/software-engineer"
prompt  = "Read the task spec, explore the codebase, write a proposal."

[[step]]
name      = "review-plan"
workspace = { git_clone = true, branch = "{task_id}", push = true }
output    = "verdict"
max_retries = 1

[step.runtime]
type    = "claude"
model   = "sonnet"
persona = "inspired/tech-lead"

[[step.transition]]
match = "pass"
goto  = "implement"

[[step.transition]]
match = "fail"
goto  = "propose"

[[step.transition]]
match = "*"
goto  = "escalate"

[[step]]
name      = "implement"
workspace = { git_clone = true, branch = "{task_id}", push = true }
output    = "diff"
max_visits      = 3
max_visits_goto = "code-tiebreak"

[step.runtime]
type    = "claude"
model   = "sonnet"
persona = "inspired/software-engineer"
prompt  = "Implement the task following the approved proposal."

[[step]]
name      = "review-code"
workspace = { git_clone = true, branch = "{task_id}", push = true }
output      = "verdict"
max_retries = 2

[step.runtime]
type    = "claude"
model   = "sonnet"
persona = "inspired/reviewer"

[[step.transition]]
match = "pass"
goto  = "merge"

[[step.transition]]
match = "fail"
goto  = "implement"

[[step.transition]]
match = "*"
goto  = "escalate"

[[step]]
name   = "merge"
action = "merge_to_main"
workspace = { branch = "{task_id}" }
on_fail     = "implement"
max_retries = 2
```

Tiebreak steps (`plan-tiebreak`, `code-tiebreak`) are omitted above for
brevity — see [docs/workflows/code-task.toml](../workflows/code-task.toml)
for the full reference implementation.

---

## Step Fields

| Field | Required | Default | Description |
|-------|----------|---------|-------------|
| `name` | yes | — | Step identifier, unique within the workflow |
| `runtime` | no | — | Runtime spec — the process ox-runner spawns (see Runtime). Mutually exclusive with `action` |
| `action` | no | — | In-process ox-server action (see Ox Actions). Mutually exclusive with `runtime` |
| `output` | no | — | Named output label passed to the next step via `{prev_output}` |
| `workspace` | no | — | Workspace spec (see below) |
| `max_retries` | no | engine default | Per-step retry limit |
| `max_visits` | no | — | Maximum times this step can be visited across retry loops |
| `max_visits_goto` | no | `"escalate"` | Step to jump to when `max_visits` is exceeded |
| `on_fail` | no | — | Step to jump to on failure (see Failure Handling) |
| `squash` | no | `false` | Squash branch commits into one before merging (action steps only) |

A step has either `runtime` (dispatched to a runner) or `action` (runs
in-process on ox-server). If neither is specified, the step defaults to
`runtime = { type = "claude" }`.

---

## Workspace Spec

The `workspace` field controls how the runner provisions the working
environment before running the agent.

| Field | Default | Description |
|-------|---------|-------------|
| `git_clone` | `false` | Clone the repo from ox-server's git endpoint |
| `branch` | — | Branch to check out. `{task_id}` is interpolated. Created from main if it does not exist |
| `push` | `false` | Whether the step is expected to push commits. Enables `no_commits` signal detection |
| `read_only` | `false` | Check out in detached HEAD mode. No commits allowed |

Each step gets a fresh clone. No state carries over between steps via the
filesystem — all inter-step communication happens through cx comments and
the `prev_output` value.

---

## Artifacts

Each step can declare artifacts the runtime will produce. The runtime
writes content to declared artifacts via the runtime interface. Implicit
artifacts are collected automatically by ox-runner.

```toml
[[step.artifact]]
name = "proposal"

[[step.artifact]]
name = "build-output"
```

Implicit artifacts collected on every step (no declaration needed):

| Artifact | Source | Streaming |
|----------|--------|-----------|
| `log` | Runtime stdout/stderr | yes |
| `commits` | `git log <base>..HEAD` | no |
| `cx-diff` | `.complex/` changes on the branch | no |

See [artifacts.md](artifacts.md) for the full artifact model.

---

## Transition Matching

Steps with `[[step.transition]]` blocks route execution based on the
step's output value. Transitions are evaluated in order; the first match
wins.

```toml
[[step.transition]]
match = "pass"      # prefix match — "pass:7" matches "pass"
goto  = "implement"

[[step.transition]]
match = "fail"
goto  = "propose"

[[step.transition]]
match = "*"         # catch-all
goto  = "escalate"
```

If no transition matches, execution advances to the next step in
declaration order. If no next step exists, the workflow completes.

The output value from the current step becomes `{prev_output}` for
whatever step runs next — including failure handlers. This is how a
worker receiving a retry knows what went wrong.

---

## Runtime

The `runtime` field on a step selects which runtime to use and passes
parameters to it. Runtime definitions are found via the configuration
search path — ox-runner has no hardcoded knowledge of any agent CLI.

```toml
[[step]]
name = "implement"

[step.runtime]
type    = "claude"
model   = "sonnet"
persona = "inspired/software-engineer"
prompt  = "Implement the task following the approved proposal."
```

`type` selects the runtime definition. `tty`, `env`, and `timeout` are
handled by ox-runner directly. All other fields (`model`, `persona`,
`prompt`, etc.) are defined by the runtime definition — there are no
special or common fields.

See [runtimes.md](runtimes.md) for the full runtime model: definition
format, field declarations, interpolation, file placement, the runtime
interface, and reference definitions.

---

## Ox Actions

Steps with an `action` field run in-process — no runner dispatch, no
runtime. The `action` field selects what runs.

| Action | Description |
|--------|-------------|
| `merge_to_main` | Merge the task branch to main |

Unknown actions fail the step immediately.

Action steps are executed by the herder's scheduler, not by a runner.
When the scheduler determines that the next step is an action step, it
executes the action inline and applies the result to execution state
immediately. There is no event round-trip — the action's success or
failure is processed in the same scheduling pass. This means action
steps can chain: if an action step succeeds and the next step is also
an action, both are processed in a single pass.

### `merge_to_main` — invariants

The merge step is the only path for code to land on main. These
invariants are absolute — violating any of them is a bug, not a
recoverable error.

**No commit is ever lost.** The merge step must fail loudly rather than
silently drop, overwrite, or skip any commit from either the branch or
main.

**Merge strategy — main never leaves HEAD:**
1. If the branch has exactly 1 commit ahead → fast-forward
   (`--ff-only`). Preserves the agent's commit as-is.
2. If `squash = true` and >1 commits ahead → `git merge --squash`.
   Creates a single commit on main with all commit messages
   concatenated. No branch checkout.
3. If `squash = false` and >1 commits ahead → `git merge --no-ff`.
   Creates a merge commit. No branch checkout.
4. Conflicts in any path → abort and fail the step.

**Preconditions checked before merge:**
- Worktree must be clean. Dirty worktree blocks all merges.
- Branch must have at least one commit ahead of the merge base. An empty
  branch is an error — the step produced nothing.

**Post-merge:** the worktree is updated to the new main HEAD. ox-server
then diffs `.complex/` against the previous HEAD and emits any resulting
cx events.

---

## Two-Phase Step Completion

Step completion is a two-phase protocol that prevents the herder from
advancing the workflow before the runner has pushed the branch.

```
1. Runner spawns runtime process  → step.running event
2. Runtime calls done             → runner forwards → step.done event (pending)
3. Herder ignores pending          — only reacts to step.confirmed
4. Runner detects runtime exit    → collects signals → step.signals event
5. Runner pushes branch            → git.branch_pushed event
6. Runner calls confirm            → step.confirmed event
7. Herder sees confirmed           → evaluates transitions → step.advanced
```

Failures bypass the pending phase and take effect immediately — no push
is needed for a failed step. If the runner loses connectivity before
confirming, the heartbeat goes stale and the herder re-dispatches.

---

## Step Signals

Signals are observable facts collected by the runner after the runtime
exits. They describe what actually happened — independent of what the
agent claims.

| Signal | Condition | Source |
|--------|-----------|--------|
| `no_commits` | `push = true` and HEAD did not advance | runner |
| `dirty_workspace` | Uncommitted changes remain after runtime exit | runner |
| `exited_silent` | Runtime exited without calling `done` | runner |
| `fast_exit` | Step completed in under 30 seconds | runner |
| `empty_log` | Runtime log artifact is zero bytes | runner |

**Default failure rules** — applied before transition matching.
Only `exited_silent` is a hard failure; workspace signals are
informational since the agent owns its git workflow.

| Condition | Result |
|-----------|--------|
| `exited_silent` | Step failure |
| `no_commits` | Informational — does not fail the step |
| `dirty_workspace` | Informational — does not fail the step |
| `fast_exit` | Informational only |
| `empty_log` | Informational only |

Signal-triggered failures set `error = "signal:<name>"` on the
`step.failed` event. The `on_fail` handler runs as normal.

---

## Failure Handling

Each step defines its failure behaviour via `on_fail`:

| Value | Behaviour |
|-------|-----------|
| *(absent)* | Retry the same step (up to `max_retries`, then escalate) |
| A step name | Jump to that step. `{prev_output}` carries the failure reason |
| `"escalate"` | Escalate immediately without retrying |

When `max_retries` is exhausted, the step always escalates regardless of
`on_fail`. The herder shadows the cx task and emits `execution.escalated`.
The escalation step is defined in the workflow — it may be a triage agent,
an interactive human step, or any other step type.

---

## Triggers

Triggers are the entry point — they create workflow executions in response
to conditions. There are three trigger sources.

### cx triggers

Fired when a commit lands on main that changes `.complex/` in a way that
matches a condition. ox-server derives these from `git log` diffs — no
separate event files in cx.

```toml
[[trigger]]
on       = "cx.task_ready"
tag      = "workflow:code-task"
workflow = "code-task"
```

Common cx trigger conditions:

| Condition | Description |
|-----------|-------------|
| `cx.task_ready` | Node transitions to ready with a matching tag |
| `cx.task_integrated` | Node becomes integrated |
| `cx.phase_complete` | All children of a `#phase` node are integrated |
| `cx.comment_added` | A comment with a matching tag is added to a node |
| `cx.node_created` | A node with a matching tag is created |

cx triggers can specify a `poll_interval` to fire repeatedly while the
condition holds:

```toml
[[trigger]]
on            = "cx.task_ready"
tag           = "phase"
state         = "claimed"
workflow      = "checkpoint"
poll_interval = "15m"      # fire every 15 minutes while condition is true
```

This replaces a separate cron system. When the condition becomes false
the trigger stops firing automatically.

### Workflow triggers

Fired by the workflow engine when a step or execution reaches a lifecycle
state. Used to chain workflows together.

```toml
[[trigger]]
on       = "execution.escalated"
workflow = "triage"
```

| Event | Description |
|-------|-------------|
| `execution.completed` | All steps finished successfully |
| `execution.escalated` | Retries exhausted, human intervention needed |
| `step.completed` | A named step finished |

### Pool triggers

Fired by infrastructure events. Built-in — not configurable.

| Event | Herder reaction |
|-------|----------------|
| `runner.registered` | Assign pending step if one is waiting |
| `runner.heartbeat` absent | Re-dispatch the step assigned to that runner |

---

## Execution Lifecycle

1. **Trigger fires** → herder creates execution (`{task_id}-e{N}`)
2. **Dispatch** → herder finds an idle runner, emits `step.dispatched`
3. **Execute** → runner provisions workspace, spawns runtime
4. **Complete** → `step.done` → signals → `step.confirmed`
5. **Advance** → herder evaluates transitions, dispatches next step
6. **Merge** → `merge_to_main` action step lands the branch on main
7. **cx events** → ox-server diffs `.complex/`, emits `cx.task_integrated`
8. **Done** → `execution.completed`

Each execution ID is `{task_id}-e{N}` where N is sequential per task
(`aJuO-e1`, `aJuO-e2`). A task can have multiple executions (retries,
re-runs). Each execution belongs to exactly one task.

### Two-phase event processing

The herder processes events in two phases:

**Phase 1: State update.** Each incoming event updates the herder's
local projection of the world — runner status, execution status, step
outcomes. No side-effects. No dispatching. No API calls. This phase is
identical during replay and live operation.

**Phase 2: Schedule.** After every state update (skipped during replay),
a single scheduling pass runs. It scans all running executions,
determines what action each needs (advance, retry, dispatch, nothing),
and executes those actions. Only one scheduling pass runs at a time.

This separation is critical. Events are facts about what happened.
The scheduler is the single place that decides what to do next. There
is no business logic in event handlers — they are pure projections.

After replay completes, the scheduler runs once. Any execution that was
mid-flight when the herder last stopped (step confirmed but not yet
advanced) is picked up and processed. This eliminates the class of bugs
where executions get stuck after a herder restart.
