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

## The Pool

The pool is the set of all registered runners. Pool size is the WIP limit
— the maximum number of steps executing concurrently.

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
```

This is a timestamp write to the runner projection — not an event in the
log. The herder checks `last_seen` on its tick. If a runner has not
heartbeated within the grace period, the herder emits
`runner.heartbeat_missed` and re-dispatches any step assigned to that
runner.

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
runner.heartbeat_missed { runner_id, last_seen, grace_period }
```

`runner.registered` — an ox-runner process has joined the pool and is
available for step assignment.

`runner.drained` — the runner has been instructed to stop accepting new
assignments and will exit after its current step completes.

`runner.heartbeat_missed` — the herder detected that a runner has not
heartbeated within the grace period. Any step assigned to that runner is
re-dispatched.
