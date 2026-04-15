# Artifacts

Every step produces artifacts. Artifacts are the observable output of a
step — logs, commits, cx activity, and any files the step declares. They
are first-class objects: stored, addressable, and streamable in real-time.

Artifacts are separated into two planes:

- **Control plane** (event stream) — notifications that artifacts exist or
  have closed. Low volume. All event stream subscribers receive these.
- **Data plane** (artifact API) — actual content. Fetched or streamed
  separately, on demand, per artifact.

This keeps the event stream lean. A subscriber watching for step transitions
is not burdened by log chunks from unrelated executions.

---

## Implicit Artifacts

Every step produces these automatically. No declaration needed in the
workflow TOML.

| Name | Source | Streaming |
|------|--------|-----------|
| `log` | Agent stdout/stderr | yes |
| `commits` | `git log <base>..HEAD` at step completion | no |

`log` is always streamed live. `commits` is collected once after the
agent exits and the branch is pushed.

Source-specific side-effect artifacts (a `cx-diff` capturing
`.complex/` changes, a GitHub PR metadata blob, a Linear ticket
transition log) are collected by source-specific runtimes or
workflow steps, not by the core runner — they are declared like any
other workflow artifact below.

---

## Declared Artifacts

Steps can declare additional artifacts in the workflow TOML:

```toml
[[step.artifact]]
name = "proposal"

[[step.artifact]]
name = "review"
```

The runtime writes content to declared artifacts via the runtime interface
(see [runtimes.md](runtimes.md)). ox-runner forwards artifact content
to the ox-server artifact API. All declared artifacts are streaming —
content is available in real-time as the runtime writes it.

---

## Artifact Lifecycle

### On dispatch

When a step is dispatched, ox-server emits `artifact.declared` for each
artifact the step will produce (implicit + declared). Subscribers learn
what to expect before the step runs.

```json
{
  "type": "artifact.declared",
  "data": {
    "execution_id": "aJuO-e1",
    "step": "implement",
    "artifact": "log",
    "source": "log",
    "streaming": true
  }
}
```

### During execution

For streaming artifacts, ox-runner writes content chunks to the artifact API
as they arrive. This is a direct write to ox-server storage — not routed
through the event stream.

### On completion

When the step completes, ox-server emits `artifact.closed` for each
artifact. Non-streaming artifacts are written at this point.

```json
{
  "type": "artifact.closed",
  "data": {
    "execution_id": "aJuO-e1",
    "step": "implement",
    "artifact": "log",
    "size": 142438,
    "sha256": "e3b0c44298fc..."
  }
}
```

---

## Artifact API

Artifact content is fetched through a separate API, not through the event
stream.

Artifacts are stored per step attempt. All endpoints accept an optional
`?attempt=N` parameter. When omitted, the latest attempt is assumed.
See [execution.md](execution.md#execution-state) for the attempt model.

### Fetch a completed artifact

```
GET /api/executions/{id}/steps/{step}/artifacts/{name}
```

Returns the full artifact content. Available once `artifact.closed` has
been emitted. Returns 404 if the artifact does not exist or is not yet
closed.

### Stream a live artifact

```
GET /api/executions/{id}/steps/{step}/artifacts/{name}/stream
```

SSE stream of artifact content as it is written. For streaming artifacts,
content arrives in real-time during step execution. For non-streaming
artifacts, this endpoint waits and returns the full content once the
artifact closes.

Reconnect with `Last-Event-ID` to resume from a specific byte offset.

### List artifacts for a step

```
GET /api/executions/{id}/steps/{step}/artifacts
```

Returns the declared artifacts for the step with their current state
(`pending`, `streaming`, `closed`) and, for closed artifacts, size and
checksum.

---

## Writing Artifact Content

ox-runner writes artifact content directly to the artifact API — not through
the event log.

```
POST /api/executions/{id}/steps/{step}/artifacts/{name}/chunks
```

Request body is raw bytes (or chunked transfer encoding for streaming
writes). ox-server appends to the artifact and makes the content available
on the stream endpoint immediately.

This endpoint is internal — used only by ox-runner. It is not part of the
public API surface.

---

## Observing Artifacts

A typical UI flow for watching a running step:

1. Subscribe to `GET /api/events/stream`
2. Receive `artifact.declared` for `log` (streaming: true)
3. Open a second connection: `GET /api/executions/{id}/steps/{step}/artifacts/log/stream`
4. Render log content as chunks arrive
5. Receive `artifact.closed` on the event stream — log is complete
6. Fetch `commits` via the fetch endpoint

Two connections with different concerns: the event stream for lifecycle
notifications, the artifact stream for content.

---

## Events

Events emitted by the artifact subsystem. Content is never carried in
events — only notifications. All events follow the common envelope defined
in [events.md](events.md).

```
artifact.declared   { execution_id, step, artifact, source, streaming }
artifact.closed     { execution_id, step, artifact, size, sha256 }
```

`artifact.declared` — emitted at step dispatch for each artifact the step
will produce. `source` is `log`, `git-commits`, or `file`. `streaming`
indicates whether content will be available live via the stream
endpoint before the artifact closes.

`artifact.closed` — the artifact is complete and fully available via the
fetch endpoint. For streaming artifacts this marks the end of the live
stream. For non-streaming artifacts, `declared` and `closed` arrive
together at step completion.

