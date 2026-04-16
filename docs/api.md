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

Update the runner's `last_seen` timestamp and current step. Response: 204.

```json
{
  "execution_id": "aJuO-e1",
  "step": "implement",
  "attempt": 1
}
```

All fields are optional — an idle runner sends null/omitted fields.
This writes directly to the `runners` table — not an event. See
[storage.md](storage.md). ox-server's background heartbeat checker
uses `last_seen` to detect stale runners and emit
`runner.heartbeat_missed`.

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
| `limit` | Max results (default 25) |
| `offset` | Skip N results |

Filtering by origin is not supported — an execution's origin is a
structural property, not a flat string, and ancestry queries need a
different surface. Use `GET /api/executions/{id}` to inspect a
specific execution's origin.

Response:

```json
{
  "executions": [
    {
      "id": "e-1744364800-42",
      "vars": { "task_id": "aJuO" },
      "origin": { "type": "source", "source": "cx", "kind": "node.ready", "subject_id": "aJuO" },
      "workflow": "code-task",
      "status": "running",
      "current_step": "implement",
      "created_at": "2026-04-04T12:00:00Z"
    }
  ],
  "total": 1,
  "offset": 0,
  "limit": 25
}
```

#### `GET /api/executions/{id}`

Full execution detail with step attempt history.

Response:

```json
{
  "id": "e-1744364800-42",
  "vars": { "task_id": "aJuO" },
  "origin": { "type": "cx_node", "node_id": "aJuO" },
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

Create an execution. Called by the herder when a trigger fires, or
manually via ox-ctl / API.

Request:

```json
{
  "workflow": "code-task",
  "trigger": "node.ready",
  "vars": {
    "task_id": "aJuO"
  },
  "origin": {
    "type": "source",
    "source": "cx",
    "kind": "node.ready",
    "subject_id": "aJuO"
  }
}
```

`vars` is optional (defaults to `{}`). The server validates vars against
the workflow's `[workflow.vars]` declarations — rejects if a required var
is missing, fills defaults for omitted optional vars.

`origin` is optional. If omitted, defaults to
`{ "type": "manual", "user": null }`. Accepted shapes:

```json
{ "type": "source",
  "source": "cx",
  "kind": "node.ready",
  "subject_id": "aJuO" }
{ "type": "execution",
  "parent_execution_id": "e-1744364800-41",
  "parent_step": "review-plan",
  "kind": "step_completed" }
{ "type": "manual", "user": "alice" }
```

`execution_id` is server-generated (`e-{epoch}-{seq}`), not derived from
any input field.

Response (201):

```json
{
  "execution_id": "e-1744364800-42"
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

#### `POST /api/executions/{id}/steps/{step}/running`

Report that the runtime process has started. Called by ox-runner after
spawning the process.

Request:

```json
{
  "attempt": 1,
  "connect_addr": "192.168.1.5:43210"
}
```

`connect_addr` is optional. Present only for interactive steps
(`tty = true`) — it is the TCP address of the PTY bridge that clients
connect to. For standard (non-tty) steps this field is omitted.

Response: 204. Appends `step.running`.

#### `GET /api/executions/{id}/steps/{step}/pty`

WebSocket upgrade. Client-side PTY relay for interactive steps.

Binary frames received = PTY output. Binary frames sent = PTY input.
Used by `ox-ctl exec attach`. If no runner session exists for this
step, the websocket is closed immediately.

#### `GET /api/executions/{id}/steps/{step}/pty/runner`

WebSocket upgrade. Runner-side PTY relay for interactive steps.

The runner connects here after spawning a PTY process. Binary frames
sent = PTY output. Binary frames received = client input (forwarded
to PTY). One runner per session. The relay session is created on
connect and removed on disconnect.

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

### Watchers

Watcher processes push source events to ox-server via the ingest
endpoint. The server stores an opaque cursor per source and
deduplicates by `(source, idempotency_key)`. See
[prd/event-sources.md](prd/event-sources.md) and
[event-sources-design.md](event-sources-design.md) for the full design.

#### `GET /api/watchers`

List all known watcher cursors for operator status. Returns one row
per source that has posted (or attempted to post) at least once.

Response:

```json
[
  {
    "source": "cx",
    "cursor": "d59b010abc...",
    "updated_at": "2026-04-14T12:00:00Z",
    "updated_seq": 42,
    "last_error": null
  }
]
```

The `cursor` field is returned verbatim for operator inspection. The
server does not interpret it.

#### `GET /api/watchers/{source}/cursor`

Read the current cursor for a single source. Called by a watcher on
startup to resume from its last committed position.

Response:

```json
{ "cursor": "d59b010abc...", "updated_at": "2026-04-14T12:00:00Z" }
```

First-boot (no row yet) returns 200 with `null` fields:

```json
{ "cursor": null, "updated_at": null }
```

The watcher treats a `null` cursor as "cold start" and seeds current
actionable state from the source before advancing.

#### `POST /api/events/ingest`

Accept a batch of source events from a watcher. Commits cursor
advancement, event appends, and idempotency records in one transaction.

Request:

```json
{
  "source": "cx",
  "cursor_before": "d59b010abc...",
  "cursor_after":  "a1b2c3d4e5...",
  "events": [
    {
      "kind": "node.ready",
      "subject_id": "Q6cY",
      "idempotency_key": "Q6cY:node.ready:a1b2c3d",
      "data": {
        "title": "ccstat models — model-mix breakdown over time",
        "node_id": "Q6cY",
        "state": "ready",
        "tags": ["workflow:code-task"]
      }
    }
  ]
}
```

| Field                      | Required | Purpose                                                                |
|----------------------------|----------|------------------------------------------------------------------------|
| `source`                   | yes      | Watcher identifier; names the `watcher_cursors` row                    |
| `cursor_before`            | yes\*    | CAS guard — must match stored cursor; `null` on first call             |
| `cursor_after`             | yes      | New cursor value to persist on success                                 |
| `events`                   | yes      | Array of events observed (may be empty)                                |
| `events[].kind`            | yes      | Source-native event kind (e.g. `node.ready`, `issue.labeled`)          |
| `events[].subject_id`      | yes      | Source-native correlation key                                          |
| `events[].idempotency_key` | yes      | Dedup key; duplicates are dropped silently                             |
| `events[].data`            | no       | Free-form payload, available to trigger matching and var templates     |

\* `cursor_before` is `null` on the very first call for a new source.

Each event is appended as a canonical envelope stamped with
`source = batch.source` and the supplied `kind`/`subject_id`/`data`.
`idempotency_key` lives only in the `ingest_idempotency` table — it
is not persisted on the envelope.

Transaction:

1. `SELECT cursor FROM watcher_cursors WHERE source = ?`. On mismatch
   against `cursor_before` → 409, no other writes; `last_error` is
   stashed on the row.
2. For each event: `INSERT OR IGNORE INTO ingest_idempotency`. No-op
   inserts skip the event (silent dedup).
3. Append non-duplicate events as canonical envelopes.
4. `INSERT OR REPLACE INTO watcher_cursors` with the new cursor,
   `updated_at`, `updated_seq`, and `last_error = NULL`.
5. Commit. Apply projections and broadcast SSE.

Response codes:

| Code | Meaning                                                                  |
|------|--------------------------------------------------------------------------|
| 200  | Batch committed (cursor advanced; 0+ events appended)                    |
| 400  | Malformed JSON, missing required fields, invalid `cursor_after`          |
| 409  | `cursor_before` CAS mismatch — caller must re-`GET` and retry            |
| 500  | Disk failure, etc.                                                       |

An empty `events` array with an advancing `cursor_after` is a valid
"I looked, found nothing, here's how far I got." An empty array with
`cursor_after == cursor_before` is a liveness ping — updates only
`updated_at`.

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

Source-specific projections (cx node state, Linear issue state, ...)
are not served by ox-server — workflow vars carry the snapshot they
need out of the triggering source event's `data` field, and
up-to-date source state lives in the source system itself.

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

Manual triggering is not served by ox-server — the herder evaluates
triggers itself against every event on the bus, watcher-emitted or
ox-internal alike. To fire a workflow by hand, synthesize an event
via `POST /api/events/ingest` or mutate the underlying source (`cx`
node, Linear issue, ...) and let the watcher observe it.

#### `POST /api/triggers/failed`

Append a `trigger.failed` event to the event log. Used by the herder
to report its own `build_vars` failures. Operators normally do not
call this directly — it exists because the herder cannot touch the
event bus except through the API.

Request: a `TriggerFailedData` payload:

```json
{
  "source_seq": 42,
  "on": "node.ready",
  "workflow": "consultation",
  "reason": {
    "type": "missing_event_field",
    "path": "event.bogus"
  }
}
```

`reason` is tagged with a `"type"` field and may be one of:

- `{ "type": "missing_event_field", "path": "..." }`
- `{ "type": "validation_failed", "message": "..." }`
- `{ "type": "unknown_workflow" }`

Response: 204. Appends `trigger.failed`.

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

List loaded workflow definitions. Triggers that target each workflow are
included for convenience (triggers are defined separately in trigger files,
not inside workflow definitions).

Response:

```json
[
  {
    "name": "code-task",
    "steps": ["propose", "review-plan", "implement", "review-code", "merge"],
    "triggers": [
      {
        "on": "node.ready",
        "source": "cx",
        "workflow": "code-task",
        "where": { "data.tags": { "contains": "workflow:code-task" } },
        "vars": { "task_id": "{event.subject_id}" }
      }
    ]
  }
]
```

---

### Status

#### `POST /api/config/reload`

Reload configuration from disk. Re-reads all workflow, runtime,
persona, and trigger files from the search path, validates them,
and swaps the live config atomically. If validation fails, the old
config is kept and errors are returned.

Response (success):

```json
{
  "status": "ok",
  "workflows": 5,
  "runtimes": 3,
  "personas": 3,
  "triggers": 1
}
```

Response (failure, `422 Unprocessable Entity`):

```json
{
  "status": "error",
  "errors": ["persona 'test/eng': sets var 'modle' which runtime 'claude' does not declare"]
}
```

#### `POST /api/config/check`

Validate configuration files without applying. Returns validation
errors (if any) and a diff of what would change compared to the
currently loaded config.

Response (valid):

```json
{
  "valid": true,
  "changes": {
    "workflows": { "added": ["new-wf"], "removed": [] },
    "runtimes": { "added": [], "removed": [] },
    "personas": { "added": ["test/new"], "removed": [] }
  }
}
```

Response (invalid):

```json
{
  "valid": false,
  "errors": ["persona 'test/eng': sets var 'modle' which runtime 'claude' does not declare"]
}
```

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
    // Watchers
    .route("/api/watchers", get(list_watchers))
    .route("/api/watchers/{source}/cursor", get(get_watcher_cursor))
    .route("/api/events/ingest", post(ingest_batch))
    // Triggers
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
    // Git smart HTTP
    .route("/git/*path", any(git_handler));
```
