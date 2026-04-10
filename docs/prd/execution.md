# Execution

This document covers the pool of runners, the step execution lifecycle,
two-phase completion, step signals, and circuit breakers.

---

## The Pool

The pool is the set of registered runners. Pool size is the WIP limit —
at most one step executes per runner at a time. See [runners.md](runners.md)
for the full runner model, lifecycle, heartbeat protocol, and events.

---

## Step Execution Lifecycle

### 1. Dispatch

The herder finds an idle runner and emits `step.dispatched` with the step
spec: workflow name, step name, runtime spec (type and fields with
interpolations applied), workspace spec, and artifact declarations.

The runner receives `step.dispatched` via the SSE stream.

### 2. Workspace provisioning

The runner provisions the workspace according to the step's workspace spec:

- Clones the repo from ox-server's git endpoint (if `git_clone = true`)
- Checks out the task branch, creating it from main if it does not exist
- For read-only steps, checks out in detached HEAD mode

Each step gets a fresh clone. No filesystem state carries over from
previous steps.

### 3. Runtime execution

The dispatch payload contains a fully-resolved step spec: the command
to run, environment variables to set, files to place (with content
inline), and proxy declarations. ox-server resolves the runtime
definition, validates fields, applies interpolation, reads file
content (personas, etc.), and resolves secrets before dispatching.
The runner does not need local access to runtime definitions or the
configuration search path — it executes what it receives.

If `tty = true` in the runtime spec, a PTY is allocated. The runner does
not distinguish between an AI agent and an interactive human session —
both are processes. The TTY flag is the only difference.

The runtime communicates with ox-runner through the runtime interface
(see below). stdout/stderr are captured as the implicit `log` artifact.
Streaming artifacts declared by the runtime are forwarded to the
ox-server artifact API by ox-runner.

When the runtime process starts successfully, the runner emits
`step.running`. This distinguishes a step that is scheduled (dispatched
to a runner but not yet started) from one that is actively executing.

### 4. Runtime exit

When the runtime process exits, the runner:

1. Closes all streaming artifact writers
2. Collects step signals (see Signals below)
3. Collects non-streaming implicit artifacts (`commits`, `cx-diff`)
4. Emits `step.signals`

If the runtime called `done` before exiting, a `step.done` event was
already emitted (pending). If the runtime exited with code 0 without
calling `done`, the runner infers `done ""` (empty output) — the
workflow engine advances to the next step by declaration order. If the
runtime exited with a non-zero code without calling `done`, the runner
emits `step.failed` with `error = "signal:exited_silent"`.

### 5. Confirm

The runner pushes the branch to ox-server's git endpoint, then calls:

```
POST /api/executions/{id}/steps/{step}/confirm
```

ox-server emits `step.confirmed`. The herder now evaluates transitions
and advances the workflow.

If the push fails, the runner emits `step.failed` with the push error.
The herder re-dispatches or escalates according to the retry policy.

---

## Runtime Interface

The runtime interface is how a spawned process communicates with
ox-runner. It provides three capabilities: reporting completion (`done`),
writing artifact content (`artifact`), and reporting metrics (`metric`).
The runtime never talks to ox-server directly — ox-runner mediates
everything.

See [runtimes.md](runtimes.md) for the full specification of the runtime
interface, runtime definitions, and command templates.

---

## Two-Phase Completion

Step completion is a two-phase protocol that prevents the herder from
advancing the workflow before the branch has been pushed.

```
runtime  → done <output>          (runtime interface → ox-runner)
             ↓
runner   → POST step.done         (ox-runner → ox-server)
             ↓
ox-server  step.done (pending) — herder ignores this
             ↓
runner   → collects signals → step.signals
runner   → pushes branch → git.branch_pushed
runner   → POST /confirm
             ↓
ox-server  step.confirmed — herder reacts
             ↓
herder   → step.advanced
```

**Failures bypass the pending phase.** If a signal triggers a failure,
or the push fails, the runner emits `step.failed` directly. No confirm
call is needed — nothing to push for a failed step.

**If the runner dies** before confirming, its heartbeat goes stale.
ox-server detects this and emits `runner.heartbeat_missed` (including the
orphaned step info from the runner's last heartbeat). The herder removes
the dead runner, transitions the execution back to `Ready`, and the
scheduler re-dispatches to a healthy runner. The branch push may or may
not have happened — the re-dispatched
step will rebase and push again.

---

## Step Signals

Signals are observable facts collected by the runner after the runtime
exits. They are independent of what the runtime claims — the runner
observes the workspace state directly.

| Signal | Condition |
|--------|-----------|
| `no_commits` | `push = true` and HEAD did not advance during the step |
| `dirty_workspace` | Uncommitted changes remain after runtime exit |
| `exited_silent` | Runtime exited without calling `done` |
| `fast_exit` | Step completed in under 30 seconds |
| `empty_log` | Runtime log artifact is zero bytes |

### Default failure rules

Applied before transition matching. Only `exited_silent` is a hard
failure — the agent never signaled completion. Other signals are
informational: the agent owns its git workflow and calls `ox-rt done`
when finished.

| Condition | Result |
|-----------|--------|
| `exited_silent` | Step failure — `error = "signal:exited_silent"` |
| `no_commits` | Informational — recorded but does not fail the step |
| `dirty_workspace` | Informational — recorded but does not fail the step |
| `fast_exit` | Informational only |
| `empty_log` | Informational only |

Signal-triggered failures emit `step.failed` and the `on_fail` handler
runs as normal.

### Signals and artifacts

The signals `no_commits` and `dirty_workspace` are derivable from the
implicit artifacts: if the `commits` artifact is empty and `push = true`,
that is `no_commits`. If the `cx-diff` or workspace contains uncommitted
changes, that is `dirty_workspace`. The signal system and artifact system
share the same underlying observations.

---

## Circuit Breakers

Each workflow step has a retry budget. The engine default is 3 attempts,
configurable per step with `max_retries`.

When retries are exhausted:

1. The herder shadows the cx task (`cx shadow`)
2. `execution.escalated` is emitted
3. The workflow's escalation step runs — this may be a triage agent, an
   interactive human step, or any other step. What happens is defined by
   the workflow, not by the infrastructure.

### max_visits

`max_visits` limits how many times a step can be visited across the full
execution — including visits caused by review loops sending execution back
to an earlier step. When `max_visits` is reached, execution jumps to
`max_visits_goto` (default: `"escalate"`).

This prevents infinite review loops where a reviewer keeps sending work
back to an implementer. After N visits, a tiebreaker step runs.

### Step Timeouts

Steps with a `timeout` in their runtime spec are monitored by
ox-server's pool manager. If a step has been dispatched for longer than
its timeout, the server emits `step.timeout`. The herder treats this as
a step failure — the normal retry and `on_fail` logic applies.

Step timeouts are independent of runner health. A runner can be healthy
(heartbeating normally) while a step is stuck (agent in an infinite
loop). The herder re-dispatches the step to another runner without
burning a retry — infrastructure failures are not workflow failures.

---

## Interactive Steps

Steps with `tty = true` are interactive — a human works in the session
rather than an AI agent. The runner allocates a TTY and the process runs
until `done` is called via the runtime interface (or the session times out).

From the runner's perspective, interactive and agent steps are identical.
Signals, artifacts, two-phase completion, and the branch workflow all
apply equally. User interactions — approving plans, giving feedback,
adjusting cx state — happen through interactive steps on a branch, not
through direct writes to main.

---

## Execution State

An execution is the full run of a workflow for a task. It tracks the
current position in the step graph and the history of every step
attempt.

### Structure

An execution is an **ordered sequence of step attempts**. This is the
primary view — it captures the full path the execution took through the
workflow graph, including loops and retries.

```
#  STEP          ATTEMPT  OUTPUT     TRANSITION
1  propose       1        proposed   → review-plan
2  review-plan   1        fail       fail → propose
3  propose       2        proposed   → review-plan
4  review-plan   2        pass       pass → implement
5  implement     1        (running)
```

Each entry in the sequence is a step attempt. The attempt number tracks
how many times that particular step has been visited. The transition
shows what output matched and where execution went next.

A step can be visited multiple times within a single execution. This
happens when:

- **Retries** — a step fails and is retried (up to `max_retries`)
- **Review loops** — a reviewer sends execution back to an earlier step
  (e.g. review-plan → propose)
- **Tiebreakers** — `max_visits` is exceeded and execution jumps to a
  tiebreak step

Each visit is a separate **attempt**. An attempt is a complete,
independent unit with its own:

- Runner assignment
- Artifacts (log, commits, cx-diff, declared)
- Metrics (runner-collected, proxy-collected, runtime-reported, derived)
- Signals
- Output value

No state carries over between attempts. Each attempt gets a fresh
workspace clone, a fresh runtime process, and produces its own
artifacts and metrics. The previous attempt's output is available
via `{prev_output}` interpolation in the next step's prompt — that
is the only inter-attempt communication channel.

### Addressing

Execution IDs are `{task_id}-e{N}` where N is sequential per task
(`aJuO-e1`, `aJuO-e2`). A task can have multiple executions (re-runs
after escalation, manual re-triggers).

Step attempts are addressed as `{execution_id}/{step}/{attempt}`:

```
aJuO-e1/propose/1        first visit to propose
aJuO-e1/review-plan/1    first visit to review-plan
aJuO-e1/propose/2        second visit (sent back by reviewer)
aJuO-e1/review-plan/2    second review
aJuO-e1/implement/1      first visit to implement
```

API endpoints and artifact paths use this addressing scheme. When
the attempt number is omitted, the latest attempt is assumed.

---

## Events

Events emitted by the execution subsystem. All events follow the common
envelope defined in [events.md](events.md).

Runner events (`runner.registered`, `runner.drained`,
`runner.heartbeat_missed`) are defined in [runners.md](runners.md).

### Execution events

```
execution.created   { execution_id, task_id, workflow, trigger }
execution.completed { execution_id }
execution.escalated { execution_id, step, reason }
execution.cancelled { execution_id, reason }
```

`execution.created` — a workflow has been triggered for a task. `trigger`
describes what caused it (e.g. `cx.task_ready`, `workflow.step_completed`,
`manual`).

`execution.escalated` — the execution has exhausted its retry budget. The
herder shadows the cx task. The escalation step defined in the workflow
runs next — it may be a triage agent, an interactive human step, or any
other step type.

### Step events

```
step.dispatched     { execution_id, step, attempt, runner_id, runtime }
step.running        { execution_id, step, attempt }
step.done           { execution_id, step, attempt, output }
step.signals        { execution_id, step, attempt, signals[] }
step.confirmed      { execution_id, step, attempt, metrics }
step.failed         { execution_id, step, attempt, error }
step.timeout        { execution_id, step, attempt, timeout_secs, runner_id }
step.advanced       { execution_id, from_step, to_step }
step.retrying       { execution_id, step, attempt }
```

All step events include `attempt` — the attempt number for this visit
to the step. This is how subscribers distinguish between the first and
second visit to the same step.

`step.dispatched` — the herder has assigned a step to a runner.

`step.running` — the runner has spawned the runtime process. The step
is now actively executing. Emitted immediately after successful spawn.

`step.done` — the runtime reported completion via the runtime interface.
Pending result — the herder does not advance until `step.confirmed`
arrives.

`step.signals` — observable facts collected by the runner after the
runtime exited. Emitted between `step.done` and `step.confirmed`.

`step.confirmed` — the runner has pushed the branch and verified the
result. The herder now advances the workflow.

`step.failed` — the step failed. `error` carries the reason (e.g.
`signal:no_commits`, `runtime:non_zero_exit`, `runner:push_failed`).

`step.timeout` — the step exceeded its `timeout` from the runtime spec.
Emitted by ox-server's pool manager, not the runner. The runner may
still be alive — this is about step duration, not runner health. The
herder treats it as a step failure (retry or escalate).

`step.advanced` — the herder has evaluated transitions and moved the
execution to the next step.
