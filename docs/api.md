# HTTP API & SSE

ox-server exposes a single HTTP server on one port (default 4840). All
communication — REST API, SSE event stream, git smart HTTP, and artifact
I/O — flows through this server.

For the event model, see [prd/events.md](prd/events.md). For artifact
semantics, see [prd/artifacts.md](prd/artifacts.md). For storage
details, see [storage.md](storage.md).

---

## Conventions

**Content type** — all REST endpoints accept and return `application/json`
unless noted otherwise.

**Error format** — errors are JSON objects with a `message` field and
an HTTP status code:

```json
{ "error": "execution not found: aJuO-e99" }
```

| Status | Meaning |
|--------|---------|
| 200 | Success |
| 201 | Created (registration, execution creation) |
| 400 | Bad request (validation error, unknown field) |
| 404 | Not found |
| 409 | Conflict (invalid state transition, merge conflict) |
| 500 | Internal error |

**Null fields** — absent optional fields are omitted from JSON, not
set to `null`.

**Timestamps** — ISO 8601 UTC, e.g. `"2026-04-04T12:00:00Z"`.

---

## Endpoint Inventory

### Events

#### `GET /api/events/stream`

SSE event stream. Returns `text/event-stream`.

Query parameters:

| Param | Default | Description |
|-------|---------|-------------|
| `last_event_id` | — | Resume from this sequence number (alternative to header) |

Headers:

| Header | Description |
|--------|-------------|
| `Last-Event-ID` | Resume from this sequence number. Overrides query param |

Each SSE message:

```
id: 42
event: step.confirmed
data: {"execution_id":"aJuO-e1","step":"implement","attempt":1,"metrics":{...}}

```

On connection:
1. If `Last-Event-ID` is present, replay all events with `seq > id`
2. Then stream live events as they are appended

If `Last-Event-ID` is absent, only live events are sent. To receive
the full history, connect with `Last-Event-ID: 0`.

The server sends a comment line (`: keepalive`) every 30 seconds to
prevent connection timeout.

---

### Runners

#### `POST /api/runners/register`

Register a new runner.

Request:

```json
{
  "environment": "seguro",
  "labels": { "region": "local", "profile": "default" }
}
```

Response (201):

```json
{
  "runner_id": "run-4a2f"
}
```

ox-server assigns the runner ID, appends `runner.registered`, and adds
the runner to the pool projection.

#### `POST /api/runners/{id}/heartbeat`

Update the runner's `last_seen` timestamp. No request body. Response: 204.

This writes directly to the `runners` table — not an event. See
[storage.md](storage.md).

#### `POST /api/runners/{id}/drain`

Drain a runner. Appends `runner.drained`. The runner receives this via
SSE and exits after its current step completes.

Response: 204.

---

### Executions

#### `GET /api/executions`

List executions.

Query parameters:

| Param | Description |
|-------|-------------|
| `status` | Filter: `running`, `completed`, `escalated`, `cancelled` |
| `workflow` | Filter by workflow name |
| `task` | Filter by task (cx node) ID |

Response:

```json
[
  {
    "id": "aJuO-e1",
    "task_id": "aJuO",
    "workflow": "code-task",
    "status": "running",
    "current_step": "implement",
    "created_at": "2026-04-04T12:00:00Z"
  }
]
```

#### `GET /api/executions/{id}`

Full execution detail with step attempt history.

Response:

```json
{
  "id": "aJuO-e1",
  "task_id": "aJuO",
  "workflow": "code-task",
  "status": "running",
  "current_step": "implement",
  "current_attempt": 1,
  "created_at": "2026-04-04T12:00:00Z",
  "attempts": [
    {
      "step": "propose",
      "attempt": 1,
      "runner_id": "run-a3f2",
      "status": "confirmed",
      "output": "proposed",
      "signals": [],
      "transition": "review-plan",
      "started_at": "2026-04-04T12:00:05Z",
      "completed_at": "2026-04-04T12:03:17Z"
    },
    {
      "step": "review-plan",
      "attempt": 1,
      "runner_id": "run-b1c4",
      "status": "confirmed",
      "output": "pass:7",
      "signals": [],
      "transition": "implement",
      "started_at": "2026-04-04T12:03:18Z",
      "completed_at": "2026-04-04T12:05:03Z"
    },
    {
      "step": "implement",
      "attempt": 1,
      "runner_id": "run-d2f3",
      "status": "running",
      "output": null,
      "signals": [],
      "transition": null,
      "started_at": "2026-04-04T12:05:04Z",
      "completed_at": null
    }
  ]
}
```

#### `POST /api/executions`

Create an execution. Called by the herder when a trigger fires.

Request:

```json
{
  "task_id": "aJuO",
  "workflow": "code-task",
  "trigger": "cx.task_ready"
}
```

Response (201):

```json
{
  "execution_id": "aJuO-e1"
}
```

Appends `execution.created`.

#### `POST /api/executions/{id}/cancel`

Cancel a running execution. Appends `execution.cancelled`.

Response: 204.

---

### Steps

#### `POST /api/executions/{id}/steps/{step}/dispatch`

Dispatch a step to a runner. Called by the herder.

ox-server resolves the runtime definition, interpolates fields, reads
file content (personas, etc.), and resolves secrets. The persisted
`step.dispatched` event stores the resolved spec with `secret_refs`
(names only). The SSE message to the assigned runner includes the full
resolved spec with secret values.

Request (from herder — specifies runner and step-level params):

```json
{
  "runner_id": "run-4a2f",
  "attempt": 1,
  "runtime": {
    "type": "claude",
    "model": "sonnet",
    "persona": "inspired/software-engineer",
    "prompt": "Read the task spec, explore the codebase, write a proposal."
  },
  "workspace": {
    "git_clone": true,
    "branch": "aJuO",
    "push": true
  },
  "artifacts": [
    { "name": "proposal" }
  ]
}
```

Response: 204. Appends `step.dispatched`.

The `step.dispatched` SSE event delivered to the runner includes the
fully-resolved step spec:

```json
{
  "execution_id": "aJuO-e1",
  "step": "implement",
  "attempt": 1,
  "runner_id": "run-4a2f",
  "resolved": {
    "command": ["claude", "-p", "/work/tmp/prompt.md", "--model", "sonnet"],
    "interactive_command": ["claude"],
    "tty": false,
    "env": {
      "CLAUDE_MODEL": "sonnet",
      "ANTHROPIC_API_KEY": "sk-ant-..."
    },
    "files": [
      { "content": "You are a software engineer...", "to": "CLAUDE.md" },
      { "content": "-----BEGIN OPENSSH PRIVATE KEY-----...", "to": ".ssh/id_ed25519", "mode": "0600" }
    ],
    "proxy": [
      { "env": "ANTHROPIC_BASE_URL", "provider": "anthropic", "target": "https://api.anthropic.com" }
    ]
  },
  "workspace": {
    "git_clone": true,
    "branch": "aJuO",
    "push": true
  },
  "artifacts": [
    { "name": "proposal" }
  ],
  "secret_refs": ["anthropic_api_key", "ssh_private_key"]
}
```

The `resolved` block and secret values are present in the SSE delivery
to the runner but **not** in the persisted event. The event log stores
`secret_refs` (names only) and the unresolved runtime spec.

#### `POST /api/executions/{id}/steps/{step}/done`

Report step completion. Called by ox-runner (forwarding from runtime).

Request:

```json
{
  "attempt": 1,
  "output": "proposed"
}
```

Response: 204. Appends `step.done` (pending state).

#### `POST /api/executions/{id}/steps/{step}/signals`

Report step signals. Called by ox-runner after runtime exit.

Request:

```json
{
  "attempt": 1,
  "signals": ["fast_exit"]
}
```

Response: 204. Appends `step.signals`.

#### `POST /api/executions/{id}/steps/{step}/confirm`

Confirm step completion. Called by ox-runner after pushing the branch.

Request:

```json
{
  "attempt": 1,
  "metrics": {
    "runner": {
      "duration_ms": 245000,
      "exit_code": 0,
      "cpu_ms": 18200,
      "memory_peak_bytes": 524288000
    },
    "runtime": {
      "input_tokens": 14523,
      "output_tokens": 3847,
      "model": "sonnet",
      "api_calls": 12
    },
    "derived": {
      "commits": 3,
      "lines_added": 247,
      "lines_removed": 89,
      "files_changed": 8
    }
  }
}
```

Response: 204. Appends `step.confirmed`.

#### `POST /api/executions/{id}/steps/{step}/fail`

Report step failure. Called by ox-runner.

Request:

```json
{
  "attempt": 1,
  "error": "signal:no_commits"
}
```

Response: 204. Appends `step.failed`.

#### `POST /api/executions/{id}/steps/{step}/advance`

Advance to the next step. Called by the herder after evaluating
transitions.

Request:

```json
{
  "from_step": "review-plan",
  "to_step": "implement"
}
```

Response: 204. Appends `step.advanced`.

---

### State Projections

#### `GET /api/state/pool`

Current runner registrations and assignments.

Response:

```json
{
  "runners": [
    {
      "id": "run-4a2f",
      "environment": "seguro",
      "labels": { "region": "local" },
      "status": "executing",
      "current_step": "aJuO-e1/implement/1",
      "last_heartbeat": "2026-04-04T12:05:30Z",
      "registered_at": "2026-04-04T11:00:00Z"
    }
  ]
}
```

#### `GET /api/state/executions`

Active and recent executions. Same format as `GET /api/executions`.

#### `GET /api/state/cx`

Current cx node states (mirrors `.complex/` on main).

Response:

```json
{
  "nodes": [
    {
      "id": "aJuO",
      "title": "Add rate limiting to the API",
      "state": "claimed",
      "tags": ["workflow:code-task"]
    }
  ]
}
```

---

### Artifacts

#### `GET /api/executions/{id}/steps/{step}/artifacts`

List artifacts for a step. Optional `?attempt=N` (defaults to latest).

Response:

```json
[
  {
    "name": "log",
    "source": "log",
    "streaming": true,
    "status": "closed",
    "size": 142438,
    "sha256": "e3b0c44298fc..."
  },
  {
    "name": "proposal",
    "source": "file",
    "streaming": true,
    "status": "closed",
    "size": 3482,
    "sha256": "a1b2c3d4e5f6..."
  }
]
```

#### `GET /api/executions/{id}/steps/{step}/artifacts/{name}`

Fetch a complete artifact. Optional `?attempt=N`.

Response: raw bytes with `Content-Type: application/octet-stream`.
Returns 404 if the artifact is not yet closed.

#### `GET /api/executions/{id}/steps/{step}/artifacts/{name}/stream`

Stream artifact content as SSE. Each message is a chunk of bytes
(base64-encoded in the SSE data field).

```
id: 0
data: SGVsbG8gd29ybGQ=

id: 1024
data: TW9yZSBjb250ZW50...

```

The `id` field is the byte offset. Reconnect with `Last-Event-ID` to
resume from a specific offset.

When the artifact closes, a final message is sent:

```
event: closed
data: {"size": 142438, "sha256": "e3b0c44298fc..."}

```

#### `POST /api/executions/{id}/steps/{step}/artifacts/{name}/chunks`

Write artifact content. Internal endpoint — used only by ox-runner.

Request: raw bytes in the body, or chunked transfer encoding.

Query parameters:

| Param | Required | Description |
|-------|----------|-------------|
| `attempt` | yes | Attempt number |
| `offset` | yes | Byte offset within the artifact |

Response: 204.

#### `POST /api/executions/{id}/steps/{step}/artifacts/{name}/close`

Close an artifact. Internal endpoint — used only by ox-runner.

Request:

```json
{
  "attempt": 1,
  "size": 142438,
  "sha256": "e3b0c44298fc..."
}
```

Response: 204. Appends `artifact.closed`.

---

### Metrics

#### `GET /api/executions/{id}/steps/{step}/metrics`

All metrics for the latest attempt. Optional `?attempt=N`.

Response: see [prd/metrics.md](prd/metrics.md) for the full response
shape.

---

### Triggers

#### `POST /api/triggers/evaluate`

Evaluate triggers for a cx node. Called by the herder on cx events, or
by ox-ctl for manual triggering.

Request:

```json
{
  "node_id": "aJuO",
  "force": false
}
```

Response:

```json
{
  "fired": [
    {
      "workflow": "code-task",
      "execution_id": "aJuO-e1"
    }
  ]
}
```

---

### Secrets

#### `PUT /api/secrets/{name}`

Set a secret. Creates or updates.

Request:

```json
{
  "value": "sk-ant-api03-..."
}
```

Response: 204. Appends `secret.set`.

#### `GET /api/secrets`

List secret names. Never returns values.

Response:

```json
[
  { "name": "anthropic_api_key" },
  { "name": "github_token" },
  { "name": "ssh_private_key" }
]
```

#### `DELETE /api/secrets/{name}`

Delete a secret.

Response: 204. Appends `secret.deleted`. Returns 404 if the secret
does not exist.

---

### Workflows

#### `GET /api/workflows`

List loaded workflow definitions.

Response:

```json
[
  {
    "name": "code-task",
    "description": "Propose → review plan → implement → review code → merge",
    "steps": ["propose", "review-plan", "implement", "review-code", "merge"],
    "triggers": [
      { "on": "cx.task_ready", "tag": "workflow:code-task" }
    ]
  }
]
```

---

### Status

#### `GET /api/status`

Server health check.

Response:

```json
{
  "status": "healthy",
  "uptime_seconds": 190800,
  "pool_size": 3,
  "pool_executing": 2,
  "pool_idle": 1,
  "executions_running": 2,
  "executions_escalated": 0,
  "workflows_loaded": 8,
  "event_seq": 4217
}
```

---

## Git Smart HTTP

ox-server serves the git smart HTTP protocol on `/git/*`. This allows
ox-runner to clone and push using standard git commands over HTTP.

The managed repository is a bare repo at `$OX_DATA/repo/`.

### Endpoints

#### `GET /git/info/refs?service=git-upload-pack`

Discovery endpoint for clones and fetches.

Response: `application/x-git-upload-pack-advertisement`

#### `POST /git/git-upload-pack`

Pack negotiation for clones and fetches.

Request/Response: `application/x-git-upload-pack-request` /
`application/x-git-upload-pack-result`

#### `GET /git/info/refs?service=git-receive-pack`

Discovery endpoint for pushes.

Response: `application/x-git-receive-pack-advertisement`

#### `POST /git/git-receive-pack`

Pack reception for pushes.

Request/Response: `application/x-git-receive-pack-request` /
`application/x-git-receive-pack-result`

### Implementation

ox-server delegates to `git http-backend` via CGI, or implements the
protocol directly using `git2` (libgit2). The CGI approach is simpler
and handles all edge cases; the `git2` approach avoids a subprocess per
request.

Recommended for v1: shell out to `git http-backend` with the
appropriate `GIT_PROJECT_ROOT` and `GIT_HTTP_EXPORT_ALL` environment
variables. This is a well-tested code path in git.

### Push Hooks

When a branch is pushed, ox-server needs to:

1. Record the push (emit `git.branch_pushed` if associated with a step)
2. If the push is to main — reject it. Only `merge_to_main` advances
   main.

This is implemented via a `post-receive` hook in the bare repo, or by
inspecting the receive-pack results after the push completes.

---

## Router Structure

```rust
let app = Router::new()
    // Events
    .route("/api/events/stream", get(sse_handler))
    // Runners
    .route("/api/runners/register", post(register_runner))
    .route("/api/runners/{id}/heartbeat", post(heartbeat))
    .route("/api/runners/{id}/drain", post(drain_runner))
    // Executions
    .route("/api/executions", get(list_executions).post(create_execution))
    .route("/api/executions/{id}", get(get_execution))
    .route("/api/executions/{id}/cancel", post(cancel_execution))
    // Steps
    .route("/api/executions/{id}/steps/{step}/dispatch", post(dispatch_step))
    .route("/api/executions/{id}/steps/{step}/done", post(step_done))
    .route("/api/executions/{id}/steps/{step}/signals", post(step_signals))
    .route("/api/executions/{id}/steps/{step}/confirm", post(step_confirm))
    .route("/api/executions/{id}/steps/{step}/fail", post(step_fail))
    .route("/api/executions/{id}/steps/{step}/advance", post(step_advance))
    // Step logs
    .route("/api/executions/{id}/steps/{step}/log/chunk", post(push_log_chunk))
    .route("/api/executions/{id}/steps/{step}/log", get(get_step_log))
    // Artifacts
    .route("/api/executions/{id}/steps/{step}/artifacts", get(list_artifacts))
    .route("/api/executions/{id}/steps/{step}/artifacts/{name}", get(get_artifact))
    .route("/api/executions/{id}/steps/{step}/artifacts/{name}/stream", get(stream_artifact))
    .route("/api/executions/{id}/steps/{step}/artifacts/{name}/chunks", post(write_artifact_chunk))
    .route("/api/executions/{id}/steps/{step}/artifacts/{name}/close", post(close_artifact))
    // Metrics
    .route("/api/executions/{id}/steps/{step}/metrics", get(get_metrics))
    // Triggers
    .route("/api/triggers/evaluate", post(evaluate_triggers))
    // Secrets
    .route("/api/secrets", get(list_secrets))
    .route("/api/secrets/{name}", put(set_secret).delete(delete_secret))
    // Workflows
    .route("/api/workflows", get(list_workflows))
    // Status
    .route("/api/status", get(status))
    // State projections
    .route("/api/state/pool", get(get_pool_state))
    .route("/api/state/executions", get(get_executions_state))
    .route("/api/state/cx", get(get_cx_state))
    // Git smart HTTP
    .route("/git/*path", any(git_handler));
```
