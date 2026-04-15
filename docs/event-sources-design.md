# Event Sources — Implementation Design

Companion to [prd/event-sources.md](prd/event-sources.md). The PRD
states the problem and the target shape (watchers push, server ingests,
server stays source-agnostic). This document describes how that change
lands in the current tree: the data shapes, the module edits, the new
crate, the HTTP surface, and the order of operations.

Scope is **ingestion only**. Runner-side mutation (the ox-runner VM
shelling out to `cx`) is out of scope and has its own follow-up PRD.

There is no core "task" abstraction in this design. Ox has source
events, workflow executions, steps, branches, and artifacts. A source
event may be about a cx node, GitHub issue, Linear ticket, timer tick, or
webhook payload. The source event triggers a workflow execution; after
that, the execution's branch and artifacts are how steps share state.

---

## Current shape (baseline)

The tree today has one hardcoded source:

- `ox-server/src/cx.rs` — shells to `cx log --json --since <sha>`,
  derives events, implements at-least-once semantics.
- `ox-server/src/main.rs:165` — `cx_poll_loop()`, spawned at server
  startup, 10s interval, reads/writes `CX_CURSOR_KEY = "cx_log_cursor"`
  in the server KV table.
- `ox-core/src/events.rs:17` — `EventType::Cx{TaskReady, TaskClaimed,
  TaskIntegrated, TaskShadowed, CommentAdded, PhaseComplete}`, each
  with its own `*Data` struct.
- `ox-core/src/workflow.rs:203` — `TriggerDef { on, tag, state,
  workflow, vars }`; `on` is a dotted event-type string like
  `"cx.task_ready"`.
- `ox-herder/src/herder.rs:941` —
  `evaluate_triggers_for_node_with_state()`: matches on `trigger.on ==
  event_type`, then `tag`, then dedup/state.
- `ox-ctl/src/up.rs:243` — spawns `ox-server`, `ox-herder`, and
  seguro-wrapped `ox-runner` instances via `spawn_detached()`.
- `ox-core/src/config.rs:234` — `OxConfig { triggers, heartbeat_grace
  }`; no concept of watchers.

The engine below the ingestion point (dispatch, pools, retries,
reviews, merges, artifacts, metrics) is already source-agnostic.
Everything we change lives above the event-bus append call.

---

## Target shape

```
 ox-cx-watcher          POST /api/events/ingest          ox-server
 ox-linear-watcher ───────────────────────────────▶      (bus, triggers,
 ox-github-watcher      GET  /api/watchers/{src}/cursor   workflows,
                   ◀───────────────────────────────       watcher_cursors)
```

ox-server exposes watcher status, cursor, and ingest endpoints. Watchers
are **stateless on disk when the source is replayable** — their cursors
live on the server in a `watcher_cursors` table, and they fetch/advance
them via HTTP. Non-replayable webhook sources need source-provided
durable delivery or a watcher-owned durable inbox. Watchers are separate
binaries launched by `ox-ctl up` from a `watchers = [...]` list in
`.ox/config.toml`. The cx integration relocates to a new crate
`ox-cx-watcher` and maps cx facts into source events.

---

## Data model changes

### 1. `SourceEvent` replaces the `Cx*` family

In `ox-core/src/events.rs`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EventType {
    // ... existing non-cx variants unchanged ...
    #[serde(rename = "source")]
    Source,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceEventData {
    pub source: String,           // "cx", "linear", "github"
    pub kind: String,             // "node.ready", "issue.labeled", ...
    pub subject_id: String,       // source-native correlation key
    pub idempotency_key: String,  // dedup key
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub data: serde_json::Value,  // free-form source payload
}
```

Wire form on the bus: a single `EventType::Source` with
`SourceEventData` in the envelope's `data` field. Consumers inspect
`data.source == "cx" && data.kind == "node.ready"` or whatever source
and kind the trigger declares.

Kinds are source-authored strings, not a closed Ox enum. Ox should not
attempt to normalize cx nodes, GitHub issues, Linear tickets, timer
ticks, or webhook payloads into a generic task lifecycle.

### 2. `EventContext::Source`

`EventContext` in `ox-core/src/events.rs:283` gets one new variant:

```rust
pub enum EventContext {
    // ... existing ...
    Source {
        source: String,
        kind: String,
        subject_id: String,
        tags: Vec<String>,
        data: serde_json::Value,
    },
}
```

`resolve(path)` extends to walk `data` for anything under
`event.data.*` via JSON pointer, and exposes top-level fields
`event.source`, `event.kind`, `event.subject_id`, and `event.tags`.

### 3. `TriggerDef` gets an optional `source`

In `ox-core/src/workflow.rs:203`:

```rust
pub struct TriggerDef {
    pub on: String,
    #[serde(default)]
    pub source: Option<String>,   // new
    pub tag: Option<String>,
    pub state: Option<String>,
    pub workflow: String,
    pub poll_interval: Option<String>,
    pub vars: HashMap<String, String>,
}
```

New syntax:

```toml
[[trigger]]
on       = "node.ready"
source   = "cx"
tag      = "workflow:code-task"
workflow = "code-task"
[trigger.vars]
branch = "cx-{event.subject_id}"
source_id = "{event.subject_id}"
```

No compatibility alias is required for this early tree. Existing
`cx.task_ready` triggers should be updated to the source/kind syntax.

### 4. `OxConfig` learns `watchers`

In `ox-core/src/config.rs:234`:

```rust
pub struct OxConfig {
    pub triggers: Vec<String>,
    pub heartbeat_grace: u64,
    #[serde(default)]
    pub watchers: Vec<String>,   // new: ["cx"], ["linear", "github"], ...
}
```

Merge semantics match `triggers`: additive across `.ox/config.toml`
files in the search path, with de-dup on name.

### 5. Server-side cursor + idempotency storage

Two new SQLite tables in `ox-server/src/db.rs`:

```sql
CREATE TABLE IF NOT EXISTS watcher_cursors (
    source       TEXT PRIMARY KEY,   -- "cx", "linear", "github"
    cursor       TEXT,                -- opaque blob; NULL before first write
    updated_at   TEXT NOT NULL,       -- wall clock of last successful ingest
    updated_seq  INTEGER,             -- bus seq of the last event appended
    last_error   TEXT                 -- last CAS/parse failure, for status UX
);

CREATE TABLE IF NOT EXISTS ingest_idempotency (
    source          TEXT NOT NULL,
    idempotency_key TEXT NOT NULL,
    first_seen_seq  INTEGER NOT NULL,
    first_seen_ts   TEXT NOT NULL,
    PRIMARY KEY (source, idempotency_key)
);
```

Both tables are written inside the **same transaction** as the
`events` append. This is the whole point of the Kafka-style
commit-with-batch shape: cursor advancement, event appends, and
idempotency records are atomic. A crash mid-batch leaves none of it
visible.

The primary-key conflict on `ingest_idempotency` implements per-event
dedup — duplicates within a batch are dropped silently, the txn
still commits, the cursor still advances. The key is `(source,
idempotency_key)` so watchers do not need globally unique idempotency
strings.

`watcher_cursors` replaces the old `CX_CURSOR_KEY` KV row. The cursor
value is opaque to the server — for cx it's a git sha, for github
it'd be a webhook delivery id, for linear a timestamp. The server
never parses it.

A periodic prune of `ingest_idempotency` (age > 30 days) is deferred;
the table grows slowly and deletion is safe.

---

## HTTP surface

Three new routes in `ox-server/src/api.rs:19`:

```rust
.route("/api/watchers",                get(list_watchers))
.route("/api/watchers/:source/cursor", get(get_watcher_cursor))
.route("/api/events/ingest",           post(ingest_batch))
```

### `GET /api/watchers`

Returns rows from `watcher_cursors` for `ox-ctl status` and future UIs:

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

The server treats `cursor` as opaque. It is returned only for operator
inspection and reset/debug workflows.

### `GET /api/watchers/:source/cursor`

```rust
async fn get_watcher_cursor(
    State(state): State<AppState>,
    Path(source): Path<String>,
) -> Result<Json<CursorResponse>, ApiError> {
    let row = state.bus.get_watcher_cursor(&source).await?;
    Ok(Json(CursorResponse {
        cursor:     row.as_ref().and_then(|r| r.cursor.clone()),
        updated_at: row.as_ref().map(|r| r.updated_at),
    }))
}
```

Missing row returns `{ cursor: null, updated_at: null }` with 200 —
the first-boot case. Watchers treat "no cursor" as a signal to
snapshot current source state rather than replay from the beginning
(see PRD §open-questions-1).

### `POST /api/events/ingest`

```rust
#[derive(Deserialize)]
struct IngestBatch {
    source: String,
    cursor_before: Option<String>,
    cursor_after:  String,
    events: Vec<SourceEventData>,
}

async fn ingest_batch(
    State(state): State<AppState>,
    Json(body): Json<IngestBatch>,
) -> Result<StatusCode, ApiError> {
    state.bus.ingest_batch(body).await
}
```

The bus method runs one transaction:

1. `SELECT cursor FROM watcher_cursors WHERE source = ?`. Compare
   against `cursor_before`. On mismatch → `ApiError::Conflict(409)`,
   stash `last_error = "cas:expected X got Y"`, no other writes.
2. For each event in `events`:
   - `INSERT OR IGNORE INTO ingest_idempotency (idempotency_key, source, ...)`.
   - If the insert was a no-op (duplicate), skip to next event.
   - Otherwise, reserve the next seq and insert an `EventType::Source`
     row into `events`.
3. `INSERT OR REPLACE INTO watcher_cursors(source, cursor, updated_at,
   updated_seq, last_error)` with `last_error = NULL` and
   `updated_seq = last_appended_seq` (or the prior value if the
   batch was empty / all-duplicates).
4. Commit.

The ingest path should not call the current single-event
`EventBus::append()` inside the transaction. It needs a batch append
primitive that locks the connection and sequence counter, inserts all
rows and cursor/idempotency updates in one SQLite transaction, commits,
then applies projections and broadcasts SSE for the committed events.
Subscribers should never observe events that later roll back.

Error codes:

| Code | Meaning                                                                 |
|------|-------------------------------------------------------------------------|
| 200  | Batch committed (cursor advanced; 0+ events appended)                   |
| 400  | Malformed JSON, missing required fields, invalid cursor_after          |
| 409  | `cursor_before` CAS mismatch — watcher must re-`GET` and retry          |
| 500  | Disk failure, etc.                                                      |

Watchers retry on 5xx with backoff; 409 is non-retryable at the
batch level (the caller must refetch and rebuild the batch). The
endpoints are unauthenticated on the loopback socket, matching the
rest of the server today.

An empty `events` array with `cursor_after != cursor_before` is a
legitimate "I looked, found nothing, here's how far I got"; with
`cursor_after == cursor_before` it's a liveness ping (updates
`updated_at`, nothing else).

---

## Trigger matching evolution

`ox-herder/src/herder.rs:941` — `evaluate_triggers_for_node_with_state()`
today is called per-Cx-event. After the change, source events and Ox's
own internal events feed the same trigger evaluation layer. For watcher
events, the matching predicate changes from:

```rust
if trigger.on != event_type { continue; }
```

to (roughly):

```rust
let SourceEventData { source, kind, tags, .. } = parse(envelope)?;
if trigger.on != kind { continue; }
if let Some(ref want) = trigger.source {
    if want != &source { continue; }
}
if let Some(ref tag_pattern) = trigger.tag {
    if !tags.iter().any(|t| t == tag_pattern) { continue; }
}
```

Dedup by `(origin, workflow)` stays exactly as today; `origin`
becomes `ExecutionOrigin::Source { source, kind, subject_id }` (new
variant) replacing `ExecutionOrigin::CxNode { node_id }`.

`EventContext::Source` feeds `trigger.build_vars()` unchanged — the
template resolver is the only place that looks inside the context, and
its new field map is listed above.

The same trigger matcher should also accept Ox internal events, such as
`execution.completed`, `execution.escalated`, `step.confirmed`,
`step.failed`, and `artifact.closed`. This is how adaptive orchestration
works without making a running workflow non-deterministic: steps create
side effects or Ox emits lifecycle facts; those facts trigger new
workflow executions through the same deterministic trigger layer.

Example:

```toml
[[trigger]]
on = "execution.completed"
workflow = "assign-work"

[trigger.vars]
completed_execution = "{event.execution_id}"
completed_workflow = "{event.workflow}"
```

State suppression (`herder.rs:983` — skip if node is `integrated` or
`shadowed`) is cx-specific and needs a rethink. For the cx watcher,
the watcher itself can filter: don't POST `node.ready` for nodes
already past `integrated`. For other sources the concept may not
apply. In this design, the suppression check moves **into the cx
watcher**, and the server-side matcher drops it entirely. This is a
small behavior shift — worth calling out for review.

---

## The `ox-cx-watcher` crate

New workspace member, dropped into `Cargo.toml` alongside the existing
six.

```
ox-cx-watcher/
├── Cargo.toml
└── src/
    ├── main.rs        # clap arg parsing, loop driver, shutdown
    ├── cx.rs          # MOVED from ox-server/src/cx.rs verbatim
    ├── client.rs      # GET cursor + POST batch with retries
    └── mapping.rs     # cx node → SourceEvent { node.ready, ... }
```

Note: **no `cursor.rs`**. There is no local cursor file. The watcher
is stateless on disk.

CLI:

```
ox-cx-watcher --server http://127.0.0.1:PORT \
              --repo /path/to/repo \
              [--interval 10s]
```

- **Cursor ownership**: server-side. On boot, `client.get_cursor("cx")`
  returns the last committed sha (or `null`). Each tick the watcher
  runs `cx log --since <cursor>`, builds a batch, and POSTs with
  `cursor_before = <cursor>`, `cursor_after = <new head sha>`. On 200
  it updates its in-memory cursor; on 409 it re-GETs and retries with
  the fresh value; on 5xx/network it backs off and retries with the
  same batch.
- **Cold start**: cursor from server is `null` → snapshot current
  `git rev-parse HEAD` and seed the current actionable state from
  `cx list --json` / `cx show`, same as today's first-boot path in
  `cx.rs:249`. This should emit source events for currently ready nodes
  that carry workflow tags, not silently skip existing work. The first
  ingest batch carries `cursor_before: null, cursor_after: <HEAD>`.
- **Mapping**: cx node state `ready|claimed|integrated` →
  `SourceEvent { source: "cx", kind: "node.{ready|claimed|done}",
  subject_id: node_id, idempotency_key: format!("{node_id}:{state}:{hash}"),
  tags: node.tags, data: node_snapshot }`. Comments become
  `comment.added` with the comment id folded into the idempotency key.
- **Retries**: exponential backoff on network/5xx failure, indefinite.
  Never advance the in-memory cursor until the server returns 200.
  At-least-once semantics are preserved because the server dedups on
  idempotency key and guards cursor advancement with CAS.
- **Buffering**: because checkpointing goes through the server, an
  extended server outage blocks cursor advancement. The watcher keeps
  its last-seen batch in memory and retries; memory use is bounded
  by the size of the cx log since the last committed cursor. On
  restart during an outage, all in-memory state is lost — but the
  next boot will re-derive the same batch from cx since the server
  cursor didn't advance. Lossless by construction.

The state suppression previously in the herder moves into
`mapping.rs`: the watcher skips events for nodes it observes as
already-shadowed/integrated based on cx's own state.

### Delete list (slice 5)

- `ox-server/src/cx.rs` — gone
- `cx_poll_loop` in `ox-server/src/main.rs:165-209` — gone
- `CX_CURSOR_KEY` in `main.rs:163` and its KV row — gone
- `EventType::Cx*` variants — gone (after log compaction window)
- `shell-out-to-cx` from anywhere under `ox-server/` — gone
- `ExecutionOrigin::CxNode` — gone, replaced by `Source`

Test: `rg '\bcx\b' ox-server/src` should return nothing but comments.

---

## `ox-ctl up` integration

`ox-ctl/src/up.rs:243` — `cmd_up()` — learns a new spawn step after
the herder spawn at `up.rs:326`. Sketch:

```rust
let hot = load_hot_config(&repo)?;
for name in &hot.config.watchers {
    let bin = bins.watcher(name)?;   // looks up ox-<name>-watcher
    let log = paths.logs.join(format!("{name}-watcher.log"));
    spawn_detached(
        &bin,
        &[
            "--server".into(), server_url.clone(),
            "--repo".into(),   repo.to_string_lossy().into(),
        ],
        &log,
    )?;
    write_pid(&paths.pids, pid, &format!("{name}-watcher"))?;
}
```

No `RunPaths.watchers` field is needed — watchers have no on-disk
state. `resolve_binaries_in()` (`up.rs:108`) learns a generic
`watcher(name)` lookup that finds `ox-{name}-watcher` in the same
directory as `ox-server`; a missing binary is a hard error at
`ox-ctl up` time with a clear message.

`ox-ctl down` reads the pidfile as today — no special casing needed
since watchers are in it. `ox-ctl status` grows a Watchers section
listing each configured watcher with: alive y/n, last-ingest-at,
last-error — all sourced from a single `GET /api/watchers` endpoint
on the server that reads the `watcher_cursors` table. No filesystem
poking, no log tailing. The cursor value itself is available too,
which makes "why hasn't this watcher moved?" a one-command answer.

---

## Migration plan — slices

The PRD lists five slices; here's how each lands as a red/green pair
in the current tree.

### Slice 1 — ingest endpoint + cursor storage (server-only, nothing calls it)

- Red: unit tests in `ox-server/src/api.rs`:
  1. `GET /api/watchers/cx/cursor` on an empty db returns `{cursor:
     null}` + 200.
  2. `POST /api/events/ingest` with `cursor_before: null,
     cursor_after: "abc", events: [e1]` returns 200, appends one
     `EventType::Source`, and updates the cursor.
  3. Same POST replayed returns 200, the idempotency table suppresses
     the event, cursor stays at `"abc"`.
  4. POST with wrong `cursor_before` returns 409 and makes no changes.
- Green:
  - Add `EventType::Source` + `SourceEventData` to
    `ox-core/src/events.rs`.
  - Add `watcher_cursors` + `ingest_idempotency` tables + migration
    in `ox-server/src/db.rs`.
  - Add `bus.get_watcher_cursor()` and `bus.ingest_batch()` +
    handlers + routes in `ox-server/src/{events,api}.rs`.
- The cx poller still runs untouched. Tree is green, ships.

### Slice 2 — generic trigger matching

- Red: unit test in `ox-herder/src/herder.rs` — build a synthetic
  `EventEnvelope { EventType::Source, data: SourceEventData { source:
  "cx", kind: "node.ready", ... } }`, pass through
  `evaluate_triggers_for_node_with_state()`, assert a matching trigger
  fires an execution with the right `vars`.
- Green:
  - Add `EventContext::Source` + `resolve()` updates in
    `ox-core/src/events.rs`.
  - Add `source` field to `TriggerDef`.
  - Extend matcher in `herder.rs` to dispatch `Source` envelopes;
    add `ExecutionOrigin::Source`.
  - Update cx triggers to `source = "cx", on = "node.ready"`.

### Slice 3 — `ox-cx-watcher` crate, in parallel with in-server poller

- Red: integration test in `ox-cx-watcher/tests/smoke.rs` — spin up an
  in-memory ox-server, point watcher at a temp repo with a cx history,
  expect the server to see the right `SourceEvent`s on the bus and
  `watcher_cursors[cx]` to equal the repo HEAD.
- Green:
  - New crate `ox-cx-watcher`; move `cx.rs` logic into it
    (copy, don't delete yet).
  - Implement mapping + HTTP client (GET cursor on boot, POST batch
    per tick with CAS retry on 409).
  - Add to workspace `Cargo.toml`.
- At this point the watcher can be tested against the ingest endpoint.
  Do not run both the legacy cx poller and the watcher as live
  trigger-producing paths for the same repo; their origins are different
  and can double-fire workflows. If both must run temporarily, run the
  watcher observe-only or disable trigger matching for one path.

### Slice 4 — switchover

- Red: integration test — run `ox-ctl up` in a fixture repo, assert
  the `ox-cx-watcher` process appears in the pidfile and the server's
  cx poller is not running.
- Green:
  - Add `watchers` to `OxConfig`; default ccstat config to `["cx"]`.
  - `ox-ctl up` spawns watchers per the new config.
  - Disable or delete `cx_poll_loop`.
  - `ox-ctl status` shows the Watchers section.

### Slice 5 — delete the legacy path

- Red: `rg '\bcx\b' ox-server/src` returns only comments (enforced by
  a test that greps).
- Green:
  - Delete `ox-server/src/cx.rs`, `cx_poll_loop`, and the
    `CX_CURSOR_KEY` KV row (but **not** the `watcher_cursors` table —
    that's the replacement and stays).
  - Delete `EventType::Cx*` and their `*Data` structs; bump db
    schema with a compaction pass that rewrites historical rows into
    `EventType::Source` form (or: declare a one-time snapshot +
    truncate, since the event log is recoverable from cx).
  - Delete `ExecutionOrigin::CxNode`.
  - Update ccstat's `triggers.toml`.

Each slice is its own commit pair (red → green). This is still early, so
the migration can favor clarity over broad backwards compatibility.

---

## Open questions, resolved or deferred

| PRD question                         | Resolution in this design                                                     |
|--------------------------------------|--------------------------------------------------------------------------------|
| 1. Cursor cold-start                 | Server returns `cursor: null` → watcher snapshots current source state and emits source events for currently actionable work. First batch carries `cursor_before: null`. |
| 2. Idempotency granularity           | `(source, idempotency_key)` PK on `ingest_idempotency`, in the same txn as the event append + cursor update. |
| 3. Watcher health surfacing          | Free from `watcher_cursors` (`updated_at`, `last_error`, current cursor). `ox-ctl status` calls a new `GET /api/watchers` endpoint. No local state files. |
| 4. Multi-watcher-per-source          | `cursor_before` CAS resolves races: two watchers racing will see one 200 and one 409 per cycle. Consumer-group-of-one per source. |
| 5. Server unavailability             | Watcher buffers the current tick's batch in memory and retries indefinitely; cursor cannot advance while the server is down. Lossless because a restart re-derives the same batch from cx. |
| 6. Replayable source requirement     | Stateless watchers are safe when the source can replay from a server-side cursor. Non-replayable webhook sources need source-provided durable delivery or a watcher-owned durable inbox. |
| 7. Runner → external mutations       | **Out of scope** (separate PRD).                                               |
| 8. Trigger backwards compatibility   | Not a priority in this early tree. Update triggers to source/kind syntax directly. |

---

## Points worth a second look before implementing

1. **State-suppression move.** The herder currently drops triggers on
   nodes that are `integrated` or `shadowed`. Moving this to the
   watcher is cleaner (source-specific logic in the source-specific
   binary), but it changes *where* the policy lives and could mask
   bugs in the watcher's own state tracking. Alternative: keep a
   generic "trigger-state-suppression" hook on the trigger matcher
   that reads `event.data.state`. Worth deciding explicitly.

2. **Event-log reset vs rewrite.** Rewriting historical `Cx*` rows
   to `Source` form is non-trivial. Since this is an early tree, a
   one-time event-log reset plus cx watcher resync is probably simpler
   than preserving every historical row.

3. **Execution origin reset.** `ExecutionOrigin::CxNode { node_id }`
   → `ExecutionOrigin::Source { source, kind, subject_id }` changes
   the serialized execution origin. Given the early stage, accept a
   one-time reset rather than carrying compatibility aliases.

4. **Trigger loops.** Adaptive orchestration happens through side
   effects and events, which means loops are possible:
   `assign-work -> node.ready -> code-task -> execution.completed ->
   assign-work`. The trigger layer needs deterministic circuit breakers:
   origin/workflow dedup, source-side state checks, visit/execution
   limits, cooldowns or poll intervals where appropriate, and clear
   event history for debugging why a workflow fired.

5. **Config hot-reload of `watchers`.** `HotConfig` in
   `ox-server/src/state.rs:23` reloads on SIGHUP. But watchers are
   launched by `ox-ctl up`, not the server — so adding a watcher to
   config requires `ox-ctl down && ox-ctl up`, not SIGHUP. Document
   this, or teach `ox-ctl` a `reload-watchers` subcommand.
