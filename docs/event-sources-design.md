# Event Sources Design

Companion to [prd/event-sources.md](prd/event-sources.md). This
document describes the watcher boundary, source-event data model,
ingest transaction, trigger matching, and `ox-cx-watcher` reference
implementation.

Scope is **ingestion only**. Runtime steps may mutate external systems
as workflow side effects; watchers observe those systems and turn their
facts into Ox source events.

There is no core "task" abstraction. Ox has source events, workflow
executions, steps, branches, and artifacts. A source event may be about
a cx node, GitHub issue, Linear ticket, timer tick, or webhook payload.
The source event can trigger a workflow execution; after that, the
execution's branch and artifacts are how steps share state.

---

## Architecture

```
 ox-cx-watcher          POST /api/events/ingest          ox-server
 ox-linear-watcher -------------------------------->     (bus, triggers,
 ox-github-watcher      GET  /api/watchers/{src}/cursor   workflows,
                   <--------------------------------      watcher_cursors)
```

Watchers are separate binaries launched by `ox-ctl up` from the
`watchers = [...]` list in `.ox/config.toml`. A watcher owns one source
system, reads its native API or local state, maps source facts into
`IngestEventData`, and posts batches (with the batch-level `source`
name) to ox-server.

ox-server stores watcher cursors, ingests source-event batches,
broadcasts committed events over SSE, and stays source-agnostic. It
does not shell out to `cx`, call Linear, call GitHub, or interpret
source cursors.

Replayable sources keep no watcher-local durable state. Their cursor
lives on the server in `watcher_cursors`, keyed by source name.
Non-replayable webhook sources need source-provided durable delivery or
a watcher-owned durable inbox before posting to Ox.

---

## Source Events

Source events land on the canonical event bus — the same envelope Ox
uses for every internal event. A watcher posts `IngestEventData`
records and ox-server stamps `source = batch.source` onto each one:

```rust
// Canonical envelope (ox_core::events)
pub struct EventEnvelope {
    pub seq: Seq,
    pub ts: DateTime<Utc>,
    pub source: String,       // "cx", "linear", "github", "schedule", ...
    pub kind: String,
    pub subject_id: String,
    pub data: serde_json::Value,
}

// What the watcher submits (per event, inside an IngestBatch)
pub struct IngestEventData {
    pub kind: String,
    pub subject_id: String,
    pub idempotency_key: String,
    pub data: serde_json::Value,
}
```

Field semantics:

| Field | Meaning |
|-------|---------|
| `source` | Watcher namespace, such as `cx`, `linear`, `github`, or `schedule` (set on the batch, not per event) |
| `kind` | Source-authored event kind, such as `node.ready`, `issue.labeled`, or `schedule.tick` |
| `subject_id` | Source-native correlation key for what the event is about |
| `idempotency_key` | Source-authored dedup key, unique within `(source, idempotency_key)`; lives in the `ingest_idempotency` table, not on the canonical envelope |
| `data` | Free-form source payload available to trigger var templates |

Kinds are source-authored strings, not a closed Ox enum. Ox does not
normalize cx nodes, GitHub issues, Linear tickets, timer ticks, or
webhook payloads into a generic lifecycle.

---

## Trigger Context

Triggers resolve `{event.*}` paths directly against the canonical
envelope — the same `EventEnvelope` the bus stores. There is no
separate "source event context" type: an ox-internal event and a
watcher source event answer the same resolver.

Resolvable paths:

| Template path | Value |
|---------------|-------|
| `{event.source}` | Watcher namespace |
| `{event.kind}` | Source-authored event kind |
| `{event.subject_id}` | Source-native subject id |
| `{event.data.<path>}` | Dotted walk through the source payload |

Leaf JSON strings, numbers, and booleans resolve to strings.
Objects, arrays, nulls, and missing paths fail interpolation and
produce a `trigger.failed` event.

---

## Trigger Syntax

Triggers match source events by event kind, optional source filter, and
generic predicates over event fields:

```toml
[[trigger]]
on       = "node.ready"
source   = "cx"
workflow = "code-task"

[trigger.where]
"data.tags" = { contains = "workflow:code-task" }

[trigger.vars]
branch = "cx-{event.subject_id}"
task_id = "{event.subject_id}"
title = "{event.data.title}"
```

`source` is optional. Omitting it lets one trigger match the same kind
from multiple watchers. `[trigger.where]` is optional. Keys are event
paths without the `event.` prefix; values are exact scalar matches or
`{ contains = "..." }` predicates for arrays and strings.

---

## Cursor Storage

Watcher cursor state lives in SQLite:

```sql
CREATE TABLE IF NOT EXISTS watcher_cursors (
    source       TEXT PRIMARY KEY,
    cursor       TEXT,
    updated_at   TEXT NOT NULL,
    updated_seq  INTEGER,
    last_error   TEXT
);

CREATE TABLE IF NOT EXISTS ingest_idempotency (
    source          TEXT NOT NULL,
    idempotency_key TEXT NOT NULL,
    first_seen_seq  INTEGER NOT NULL,
    first_seen_ts   TEXT NOT NULL,
    PRIMARY KEY (source, idempotency_key)
);
```

`watcher_cursors.cursor` is opaque to the server. For cx it is a git
sha. For GitHub it can be a delivery id. For Linear it can be a
timestamp or source-native cursor.

`ingest_idempotency` dedups events by `(source, idempotency_key)`.
Duplicate events are skipped silently while the batch transaction still
commits and the cursor still advances.

---

## HTTP Surface

`ox-server/src/api.rs` exposes three watcher routes:

```rust
.route("/api/watchers", get(list_watchers))
.route("/api/watchers/{source}/cursor", get(get_watcher_cursor))
.route("/api/events/ingest", post(ingest_batch))
```

### `GET /api/watchers`

Returns one row per known watcher cursor:

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

`ox-ctl status` uses this endpoint for watcher health. `last_error`
stores the last cursor CAS failure or ingest error recorded by the
server.

### `GET /api/watchers/{source}/cursor`

Returns the last committed cursor for one watcher:

```json
{
  "cursor": "d59b010abc...",
  "updated_at": "2026-04-14T12:00:00Z"
}
```

For a source with no cursor row, the response is:

```json
{
  "cursor": null,
  "updated_at": null
}
```

Watchers treat `cursor: null` as cold start and snapshot their current
source state.

### `POST /api/events/ingest`

Request:

```json
{
  "source": "cx",
  "cursor_before": "d59b010abc...",
  "cursor_after": "e0f42b7...",
  "events": [
    {
      "source": "cx",
      "kind": "node.ready",
      "subject_id": "Q6cY",
      "idempotency_key": "Q6cY:node.ready:e0f42b7",
      "data": {
        "node_id": "Q6cY",
        "state": "ready",
        "tags": ["workflow:code-task"],
        "title": "Implement watcher ingest"
      }
    }
  ]
}
```

Response:

```json
{
  "appended": 1,
  "deduped": 0
}
```

Error codes:

| Code | Meaning |
|------|---------|
| 200 | Batch committed |
| 400 | Malformed request |
| 409 | `cursor_before` does not match the stored cursor |
| 500 | Storage or server failure |

On 409 the watcher re-fetches the cursor and rebuilds the batch. On
5xx or network failure the watcher retries without advancing its
in-memory cursor.

---

## Ingest Transaction

`EventBus::ingest_batch()` performs one SQLite transaction:

1. Read `watcher_cursors[source]` and compare it to `cursor_before`.
2. Record `last_error` and return 409 on cursor mismatch.
3. Insert each event's `(source, idempotency_key)` into
   `ingest_idempotency` with `INSERT OR IGNORE`.
4. Append non-duplicate events as canonical envelope rows stamped
   with `source = batch.source`.
5. Upsert `watcher_cursors[source] = cursor_after`, clear
   `last_error`, and record `updated_seq`.
6. Commit.
7. Publish the new in-memory sequence counter.
8. Apply projections and broadcast committed events to SSE subscribers.

The in-memory sequence counter advances only after commit succeeds.
Subscribers never observe events that later roll back.

An empty `events` array is valid. If `cursor_after != cursor_before`,
the watcher records that it inspected the source and advanced its
cursor. If `cursor_after == cursor_before`, the batch is a liveness
ping that updates `updated_at`.

---

## Trigger Matching

The herder queues every envelope it observes on SSE (in live mode)
and matches each against loaded triggers:

```rust
if trigger.on != envelope.kind {
    continue;
}
if let Some(ref want_source) = trigger.source
    && want_source != &envelope.source
{
    continue;
}
if !trigger.matches_where(envelope) {
    continue;
}
```

Matching triggers create executions with:

```rust
ExecutionOrigin::Event {
    source: envelope.source.clone(),
    kind: envelope.kind.clone(),
    subject_id: envelope.subject_id.clone(),
    seq: envelope.seq,
}
```

Dedup is by `(origin, workflow)` via `origins_match_for_dedup`, which
compares the `(source, kind, subject_id)` triplet and ignores the
firing seq. `running` and `escalated` executions block a duplicate
auto-trigger.

`trigger.build_vars()` resolves `{event.*}` paths against the
envelope. Missing event fields, var validation failures, and unknown
workflow references append `trigger.failed`.

Matching runs on every envelope, so ox-internal events are
triggerable by the same mechanism as watcher events. A workflow that
should fire when another execution finishes declares
`on = "execution.completed"` (with an optional `source = "ox"`
filter). The bus and the matcher do not distinguish "source events"
from "ox events" — both are canonical envelopes.

Source-specific state suppression belongs in the watcher. For cx, the
watcher does not emit `node.ready` for nodes it observes as shadowed or
already integrated.

---

## `ox-cx-watcher`

`ox-cx-watcher` is the reference watcher crate:

```
ox-cx-watcher/
├── Cargo.toml
└── src/
    ├── main.rs        # clap args, tick loop, shutdown
    ├── cx.rs          # cx CLI integration and parsers
    ├── client.rs      # watcher HTTP client
    └── mapping.rs     # cx facts -> IngestEventData
```

CLI:

```bash
ox-cx-watcher --server http://127.0.0.1:4840 \
              --repo /path/to/repo \
              --interval-secs 10
```

Startup:

1. Fetch `GET /api/watchers/cx/cursor`.
2. Use `cursor: null` as cold start.
3. Use a non-null cursor as the lower bound for `cx log --since`.

Cold start:

1. Read `git rev-parse HEAD`.
2. Run `cx list --json`.
3. Emit source events for current actionable nodes.
4. Post a batch with `cursor_before: null` and `cursor_after: <HEAD>`.

Incremental tick:

1. Run `cx log --json --since <cursor>`.
2. Fetch current snapshots for touched nodes with `cx show`.
3. Map node states and comments into `IngestEventData`.
4. Post one batch with CAS.
5. Advance the in-memory cursor only after a 200 response.

Mapping:

| cx fact | Source event |
|---------|--------------|
| ready node, not shadowed | `source = "cx", kind = "node.ready"` |
| claimed node | `source = "cx", kind = "node.claimed"` |
| integrated node | `source = "cx", kind = "node.done"` |
| added comment | `source = "cx", kind = "comment.added"` |

Node events carry node tags inside `data.tags` with the node snapshot.
Comment events carry the comment tag inside `data.tag` with comment
metadata.

---

## `ox-ctl up`

`ox-ctl up` launches watcher binaries from config:

```toml
watchers = ["cx"]
```

For each watcher name, `ox-ctl` resolves a sibling binary named
`ox-<name>-watcher`, passes `--server` and `--repo`, writes its process
to the run pidfile, and logs to the run log directory.

`ox-ctl down` stops watchers through the same pidfile path as the
server, herder, and runners. `ox-ctl status` renders watcher health
from `GET /api/watchers`.
