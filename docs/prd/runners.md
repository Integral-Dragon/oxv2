# Runners

A runner is a registered ox-runner process available to execute workflow
steps. Runners are the unit of concurrency in the pool — at most one step
executes in a runner at a time.

---

## What a Runner Is

An ox-runner process registers with ox-server on startup and becomes a
runner. It remains registered for its lifetime. Steps are assigned to it,
executed, and completed — after which the runner is idle and eligible for
the next assignment.

The runner is the executor. It is analogous to a GitHub Actions runner:
it registers with the server, receives work via the event stream, executes
it, and reports results. ox-server does not know or care where the runner
is running.

---

## Runner Lifecycle

```
registered → idle → assigned → executing → idle → ...
                                          → drained → exit
```

**registered** — ox-runner has called `POST /api/runners/register` and the
runner appears in the pool. The herder may now assign steps to it.

**idle** — no step is currently assigned. The runner is eligible for
assignment.

**assigned** — the herder has dispatched a step. The runner has received
`step.dispatched` via the SSE stream and is provisioning the workspace.

**executing** — the runtime process is running.

**drained** — the herder has instructed the runner to stop accepting new
assignments. The runner completes its current step if one is running, then
exits. Drain is how the herder shrinks the pool.

The runner is released only when the ox-runner process exits. It is never
released between steps.

---

## Event Stream Replay

On startup, the runner subscribes to the SSE stream from event 0 and
replays the full history. During replay, it compacts the stream down to
its current state: a single optional pending assignment.

The runner tracks three rules to compact the stream:

- `step.dispatched` to my runner ID → sets the pending assignment
- `step.dispatched` for the same (execution, step) to a different runner → clears it (step was reassigned)
- `step.confirmed`, `step.failed`, `step.timeout` for my pending step → clears it (step completed)

After replay, if a pending assignment remains, the runner executes it
immediately. If not, it is idle. From that point forward, the runner
processes live events normally.

This is the same replay pattern used by the herder. The runner's state
is simpler — one optional assignment instead of a full execution map —
but the principle is identical: rebuild state from the event stream,
then go live. The event log is the source of truth, not any snapshot
or checkpoint.

On SSE reconnection (network drop), the runner reconnects from the last
processed seq. No full replay is needed — only events since the
disconnect.

---

## The Pool

The pool is the set of all registered runners. Pool size is the WIP limit
— the maximum number of steps executing concurrently. **ox-server owns
pool management** — registration, heartbeats, drain, and liveness
detection all happen on the server. The herder reacts to runner events
but does not manage runners directly.

ox-runner processes are started externally — by a provisioning script,
systemd unit, Kubernetes controller, or similar — and register
themselves with ox-server on startup. ox does not spawn runners. The
herder observes pool size and drains surplus runners when the pool
exceeds the configured target, but it never creates new runners.

Scaling up is an operational concern: to add capacity, start more
ox-runner processes (e.g. create more seguro VMs). Each ox-runner is
started with `OX_SERVER` pointing at the ox-server URL and registers
itself on startup. The execution environment (seguro VM, GCP VM,
Kubernetes pod, bare metal) is a deployment decision.

---

## Heartbeats

ox-runner sends periodic heartbeats to prove liveness:

```
POST /api/runners/{id}/heartbeat
{ "execution_id": "aJuO-e1", "step": "implement", "attempt": 1 }
```

Each heartbeat carries the step the runner is currently executing (or
null fields when idle). ox-server writes the timestamp and step info to
the `runners` table — not to the event log.

ox-server runs a background check every 15 seconds. If a runner's
`last_seen` is older than the configured grace period
(`--heartbeat-grace`, default 60s), the server emits
`runner.heartbeat_missed` with the runner ID and the orphaned step info
from the last heartbeat. The herder receives this event, removes the
dead runner from its pool, and transitions the orphaned execution back
to `Ready` for re-dispatch to a healthy runner.

The heartbeat_missed event is emitted at most once per stale runner.
If the runner comes back (re-registers with a fresh heartbeat), it is
treated as a new runner.

---

## Environments and Labels

The `environment` field on registration identifies where the runner is
running. `labels` are arbitrary key-value pairs.

```json
{
  "runner_id": "run-4a2f",
  "environment": "seguro",
  "labels": { "region": "local", "profile": "default" }
}
```

ox-server does not currently use environment or labels for routing — any
idle runner can receive any step. They are available for future routing
logic (e.g. routing browser steps to runners with the browser profile).

---

## Events

```
runner.registered       { runner_id, environment, labels }
runner.drained          { runner_id, reason }
runner.heartbeat_missed { runner_id, last_seen, grace_period_secs,
                          execution_id?, step?, attempt? }
```

`runner.registered` — an ox-runner process has joined the pool and is
available for step assignment.

`runner.drained` — the runner has been instructed to stop accepting new
assignments and will exit after its current step completes.

`runner.heartbeat_missed` — ox-server detected that a runner has not
heartbeated within the grace period. Includes the step the runner was
last working on (from its most recent heartbeat), so the herder can
re-dispatch without additional lookups.
