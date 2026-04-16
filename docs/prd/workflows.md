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

Workflows reference personas by name. The persona declares its own
runtime and model — the workflow doesn't need to. Personas and skills
can be referenced by ecosystem registry name (e.g.
`ox-community/senior-reviewer`) or by local name. See
[ecosystem.md](ecosystem.md) for registry resolution.

```toml
[workflow]
name        = "code-task"
description = "Propose → review plan → implement → review code → merge"
persona     = "inspired/software-engineer"    # default persona for steps

[[step]]
name      = "propose"
workspace = { git_clone = true, branch = "{branch}", push = true }
output    = "diff"
prompt    = "Read the task spec, explore the codebase, write a proposal."
max_visits      = 3
max_visits_goto = "plan-tiebreak"

[[step]]
name      = "review-plan"
persona   = "inspired/tech-lead"              # override workflow default
workspace = { git_clone = true, branch = "{branch}", push = true }
output    = "verdict"
max_retries = 1

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
workspace = { git_clone = true, branch = "{branch}", push = true }
output    = "diff"
prompt    = "Implement the task following the approved proposal."
max_visits      = 3
max_visits_goto = "code-tiebreak"

[[step]]
name      = "review-code"
persona   = "inspired/reviewer"
workspace = { git_clone = true, branch = "{branch}", push = true }
output      = "verdict"
max_retries = 2

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
workspace = { branch = "{branch}" }
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
| `persona` | no | workflow default | Persona for this step (declares runtime, model, skills) |
| `prompt` | no | `""` | Task-specific prompt passed to the runtime |
| `runtime` | no | — | Runtime overrides (model, tty, env, timeout). Escape hatch — normally the persona declares the runtime |
| `action` | no | — | In-process ox-server action (see Ox Actions). Mutually exclusive with `persona`/`runtime` |
| `output` | no | — | Named output label passed to the next step via `{prev_output}` |
| `workspace` | no | — | Workspace spec (see below) |
| `skills` | no | `[]` | Additional skills available to this step (added to runtime + persona + workflow skills) |
| `max_retries` | no | engine default | Per-step retry limit |
| `max_visits` | no | — | Maximum times this step can be visited across retry loops |
| `max_visits_goto` | no | `"escalate"` | Step to jump to when `max_visits` is exceeded |
| `on_fail` | no | — | Step to jump to on failure (see Failure Handling) |
| `squash` | no | `false` | Squash branch commits into one before merging (action steps only) |

A step has either a `persona` (dispatched to a runner) or an `action`
(runs in-process on ox-server). If a step has no persona and no action,
it inherits the workflow's default persona.

---

## Skills

Skills compose additively across four levels: runtime, persona,
workflow, and step. The agent sees the union of all.

```toml
[workflow]
name   = "data-pipeline"
skills = ["acme-corp/pg-tools", "acme-corp/grafana-reader"]

[[step]]
name    = "analyze-logs"
persona = "inspired/software-engineer"
skills  = ["acme-corp/es-query"]      # added on top of runtime + persona + workflow skills
prompt  = "Analyze the recent error logs."
```

The agent executing `analyze-logs` sees skills from:
- The runtime (e.g. `ox-skills/shell` declared in claude.toml)
- The persona (e.g. whatever `inspired/software-engineer` declares)
- The workflow (`acme-corp/pg-tools`, `acme-corp/grafana-reader`)
- The step (`acme-corp/es-query`)

Skills only add — a step cannot remove a skill granted by the runtime,
persona, or workflow level. See [skills.md](skills.md) for the full
skill model.

---

## Workspace Spec

The `workspace` field controls how the runner provisions the working
environment before running the agent.

| Field | Default | Description |
|-------|---------|-------------|
| `git_clone` | `false` | Clone the repo from ox-server's git endpoint |
| `branch` | — | Branch to check out. Execution vars are interpolated (e.g. `{branch}`, `{event.subject_id}`). Created from main if it does not exist |
| `push` | `false` | Whether the step is expected to push commits. Enables `no_commits` signal detection |
| `read_only` | `false` | Check out in detached HEAD mode. No commits allowed |

Each step gets a fresh full clone (not `--single-branch`). Agents always
have `origin/main` available for `git diff origin/main..HEAD` and
`git rebase origin/main`. No state carries over between steps via the
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

## Deterministic Control

The workflow engine never asks an agent what step should run next.
Transitions are evaluated from declared step output, signals, failure
state, retry counters, and event facts.

Agents can produce outputs such as `pass`, `fail`, `proposed`, or
`needs-human`, but the meaning of those outputs is defined by the workflow
TOML. The output value is data, not authority. This keeps orchestration
inspectable, replayable, and testable.

If a workflow needs a new path, encode that path in the workflow definition.
Agents may propose workflow changes as artifacts, but applying those changes
is a normal branch/merge operation.

---

## Personas and Runtimes

Steps name a persona. The persona declares which runtime and model to
use. This means the workflow author thinks in terms of roles ("who
does this step"), not infrastructure ("which CLI to run").

```toml
[[step]]
name    = "implement"
persona = "inspired/software-engineer"
prompt  = "Implement the task following the approved proposal."
```

The persona `inspired/software-engineer` might declare `runtime: claude`
and `model: sonnet` in its frontmatter. The workflow doesn't need to
know. Swapping to a Codex-based persona is just changing the persona
name — the workflow TOML stays the same.

For cases where a step needs to override the persona's defaults, the
`[step.runtime]` block is an escape hatch:

```toml
[[step]]
name    = "implement"
persona = "inspired/software-engineer"
prompt  = "Implement the task following the approved proposal."

[step.runtime]
model   = "opus"        # override persona's model for this step
timeout = "30m"
```

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

**Post-merge:** the worktree is updated to the new main HEAD. Any
source-specific side effects of the merge (cx state transitions,
GitHub issue updates, ...) are observed by the corresponding watcher
and arrive back as source events through the normal ingest path.

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
`on_fail`. The herder emits `execution.escalated`; source-specific
workflows may then mark source objects shadowed as a side effect (for
cx, that's a `cx shadow` call from a step). The escalation step is
defined in the workflow — it may be a triage agent, an interactive
human step, or any other step type.

---

## Triggers

Triggers are the entry point — they create workflow executions in response
to conditions. There are three trigger sources.

Triggers are defined in standalone TOML files (not inside workflow
definitions) and loaded via `config.toml`. This decouples trigger
routing from workflow logic — the same workflow can be started by
multiple triggers, and you can reuse template workflows while defining
your own triggers.

```toml
# config.toml — lists trigger files to load (paths relative to this file)
triggers = [
    "workflows/triggers.toml",
]
```

Trigger files across the search path are additive — repo-local and
default triggers all load. If no `config.toml` exists, ox falls back
to loading `workflows/triggers.toml` from each search-path directory.

### Source event triggers

Source event triggers fire when a watcher plugin ingests an event that
matches the trigger's filters. Ox has no built-in knowledge of any
particular source system — cx, Linear, GitHub, a timer — a watcher
observes each source, maps native facts into `SourceEvent` envelopes,
and posts them to `POST /api/events/ingest`. Triggers select which
events start workflows.

```toml
# workflows/triggers.toml
[[trigger]]
on       = "node.ready"                # the source-native event kind
source   = "cx"                        # watcher identifier (optional filter)
tag      = "workflow:code-task"        # must appear in the event's tag list
workflow = "code-task"
[trigger.vars]
branch  = "cx-{event.subject_id}"
task_id = "{event.subject_id}"
title   = "{event.data.title}"
```

Fields:

- `on` — matches the event's `kind` field verbatim. Kinds are
  source-authored strings like `node.ready`, `issue.labeled`, or
  `schedule.tick`; Ox does not interpret them.
- `source` — optional. When set, only events whose `source` equals
  this value match. Omit it to accept any watcher.
- `tag` — optional. When set, the event's `tags` list must contain
  this string.
- `workflow` — the workflow to start.
- `[trigger.vars]` — templates resolved against the firing event
  envelope (`{event.source}`, `{event.kind}`, `{event.subject_id}`,
  `{event.data.*}`). The resulting map is validated against the
  workflow's `[workflow.vars]` declarations.

The mapping is what lets different workflows use different var names
for the same event — `code-task` declares `task_id`, `consultation`
declares `branch`, and the triggers template whichever name the
workflow wants:

```toml
[[trigger]]
on       = "node.ready"
source   = "cx"
tag      = "workflow:consultation"
workflow = "consultation"
[trigger.vars]
branch = "cx-{event.subject_id}"
```

A trigger with no `[trigger.vars]` block produces no workflow vars —
the workflow must declare only optional vars with defaults, or the
execution will fail `validate_vars` and emit a `trigger.failed` event.

#### Event field namespace

Source events expose a small fixed envelope plus a free-form `data`
blob. Any field on either side is reachable from a `{event.*}`
template.

| Template path | Value |
|---------------|-------|
| `event.source` | Watcher identifier (`cx`, `linear`, `github`, ...) |
| `event.kind` | Source-native event kind (e.g. `node.ready`) |
| `event.subject_id` | Source-native correlation key |
| `event.tags` | Comma-joined tag list |
| `event.data.<path>` | Dotted walk into the JSON payload |

A cx watcher might populate `event.data.title`, `event.data.state`,
and `event.data.node_id`; a GitHub watcher might populate
`event.data.issue.number` and `event.data.labels`. The trigger picks
which fields to template into workflow vars.

#### Trigger failures

A trigger that cannot produce a valid execution emits a
`trigger.failed` event instead:

- **`MissingEventField { path }`** — a `[trigger.vars]` template
  referenced a field the firing event does not expose.
- **`ValidationFailed { message }`** — the interpolated vars map
  failed `WorkflowDef::validate_vars` (e.g. a required var was not
  mapped).
- **`UnknownWorkflow`** — the trigger references a workflow that is
  not loaded.

Trigger failures are deterministic and fire-once: the herder guards
emission behind `!replaying`, so a stale failure is not re-logged on
every restart. An operator fixes the TOML and re-fires manually.

Source event triggers can specify a `poll_interval` to fire
repeatedly while the condition holds:

```toml
[[trigger]]
on            = "node.ready"
source        = "cx"
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

1. **Trigger fires** → herder creates execution (synthetic ID)
2. **Dispatch** → herder finds an idle runner, emits `step.dispatched`
3. **Execute** → runner provisions workspace, spawns runtime
4. **Complete** → `step.done` → signals → `step.confirmed`
5. **Advance** → herder evaluates transitions, dispatches next step
6. **Merge** → `merge_to_main` action step lands the branch on main
7. **Source side effects** → watchers observe downstream state changes
   (cx node state transitions, GitHub issue updates, ...) and emit new
   source events that may trigger follow-up workflows
8. **Done** → `execution.completed`

Execution IDs are server-generated (`e-{epoch}-{seq}`). The same
source subject (a cx node, a GitHub issue, ...) can have multiple
executions over time through retries, re-runs, or different workflows
matching different events.

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
