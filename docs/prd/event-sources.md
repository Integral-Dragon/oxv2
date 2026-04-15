# Event Sources (Watcher Plugins)

> **Status:** Implemented. ox-server has no source-specific code;
> `ox-cx-watcher` is the reference watcher plugin that posts cx
> state changes to `/api/events/ingest`. This document stays in the
> tree as the design of record for the watcher boundary; the
> migration-plan sections below are kept as history of how the
> refactor landed.

## Problem

ox-server had one hardcoded ingestion path: **complex** (cx). A
`cx_poll_loop` ran inside ox-server, shelled out to `cx log` every
10 seconds, parsed the output, and appended cx-specific event types
(`CxTaskReady`, `CxTaskClaimed`, `CxTaskIntegrated`, `CxCommentAdded`)
to the event bus. Triggers matched on these event types to fire
workflows.

This coupled ox-server to cx. Running ox against a Linear project, a
GitHub repo's issues, Jira, or any other work tracker would have
required a second hardcoded poller — and a third, and a fourth. Every
new source was a change to ox-server itself.

The engine below the ingestion point is source-agnostic: runner
dispatch, pool management, retries, reviews, merges, artifacts,
metrics — none of it cares where the triggering event came from. Only
the ingestion path was coupled.

There is no generic "task" object in Ox. An external event may be
about a cx node, a GitHub issue, a Linear ticket, a timer tick, or a
webhook payload. Ox treats that event as a possible trigger for a
workflow execution. After the execution starts, the workflow branch,
step artifacts, logs, commits, and side effects are the shared state
between steps.

## Goal

Make event ingestion a plugin boundary. ox-server stops polling
anything; external **watcher** processes own the integration with each
source system and push events into ox-server via a single HTTP
endpoint. Anyone can write a watcher in any language; ox-server has
zero knowledge of cx, Linear, GitHub, or anything else.

The source decides what an event means. Ox stores the source, event kind,
subject identifier, tags, and payload; trigger definitions decide whether
that fact starts a workflow and how event fields become execution vars.

The test: after this change, deleting `ox-server/src/cx.rs` and
searching the ox-server source for "cx" should return nothing but
comments.

## Architecture

```
┌───────────────┐    POST /api/events/ingest   ┌──────────────┐
│ ox-cx-watcher │ ─────── event ──────────▶     │              │
├───────────────┤                               │              │
│ ox-linear-... │ ─────── event ──────────▶     │  ox-server   │
├───────────────┤                               │              │
│ ox-github-... │ ─────── event ──────────▶     │              │
└───────────────┘    GET /api/watchers/{src}/cursor  └──────┬───────┘
  (one process    ◀────── cursor ──────────                │
   per source,                                             ▼
   stateless                                          triggers →
   on disk)                                           workflows
```

Each watcher:

- Is a standalone process, launched alongside ox-server the same way
  ox-herder is.
- Talks to exactly one source system. It knows that system's API
  (polling, webhooks, subscription streams — whatever's appropriate).
- Is **stateless on disk when the source is replayable**. Its cursor
  (last git sha for cx, last durable delivery id for github, last ISO
  timestamp for linear — whatever it needs to resume) lives on the
  server, keyed by watcher name. On boot the watcher fetches its cursor;
  on each successful ingest the cursor advances atomically with the event
  append. Non-replayable webhook sources need source-provided durable
  delivery or a watcher-owned durable inbox.
- Maps source-native events into a small source-event envelope and posts
  them to ox-server's ingest endpoint.
- Is otherwise invisible to ox-server. The server never calls a
  watcher; communication is one-way.

Cursors are **opaque blobs** to the server — it stores and returns a
string without interpreting it. This keeps ox-server source-agnostic
while giving operators one place to inspect, reset, or rewind a
watcher.

## API

ox-server exposes three new endpoints:

```
GET  /api/watchers
GET  /api/watchers/{source}/cursor
POST /api/events/ingest
```

### Watcher status

```
GET /api/watchers
→ 200 [
  {
    "source": "cx",
    "cursor": "d59b010abc...",
    "updated_at": "2026-04-14T12:00:00Z",
    "updated_seq": 42,
    "last_error": null
  }
]
```

Used by `ox-ctl status` and future UIs. The cursor is an opaque string
for operator inspection; Ox does not parse it.

### Cursor read

```
GET /api/watchers/cx/cursor
→ 200 { "cursor": "d59b010abc...", "updated_at": "2026-04-14T12:00:00Z" }
→ 200 { "cursor": null,            "updated_at": null }   # first boot
```

The watcher calls this once at startup and uses the returned string
(if any) to resume. The server does not interpret the cursor — it's
whatever blob the watcher wrote last time.

### Batch ingest

```
POST /api/events/ingest
```

Body:

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
      "tags": ["workflow:code-task"],
      "data": {
        "title": "ccstat models — model-mix breakdown over time",
        "node_id": "Q6cY",
        "state": "ready"
      }
    }
  ]
}
```

Fields:

| Field             | Required | Purpose                                                              |
|-------------------|----------|----------------------------------------------------------------------|
| `source`          | yes      | Watcher identifier (`cx`, `linear`, `github`); names the cursor row  |
| `cursor_before`   | yes*     | CAS guard — must match current server-side cursor; null on first call |
| `cursor_after`    | yes      | New cursor value to persist on success                               |
| `events`          | yes      | Array of events observed in this advancement (may be empty)          |
| `events[].kind`          | yes   | Source-native event kind, such as `node.ready` or `issue.labeled` |
| `events[].subject_id`    | yes   | Source-native correlation key for what the event is about         |
| `events[].idempotency_key` | yes | Dedup key; server rejects duplicate events silently            |
| `events[].tags`          | no    | Routing labels for trigger matching                              |
| `events[].data`          | no    | Free-form payload, templatable from workflows                    |

\* `cursor_before` is compared against the stored cursor. On mismatch
the server returns 409 Conflict and makes no changes — this is how
races between concurrent watchers for the same source resolve.

On receipt, ox-server handles the whole batch in one transaction:

1. Check `cursor_before == watcher_cursors[source]`. On mismatch →
   409, no side effects.
2. For each event: dedup on `idempotency_key`. Duplicates are dropped
   silently inside the batch.
3. Append non-duplicate events to the bus as `SourceEvent`s.
4. Update `watcher_cursors[source] = cursor_after`.
5. Commit. Run trigger matching against loaded triggers.
6. Fire any matching workflow executions, same code path as today.

An empty `events` array with an advancing cursor is valid and useful:
it's how a watcher says "I looked, found nothing new, here's how far
I got." Heartbeat and cursor advancement are the same primitive.

A batch with `events: []` and `cursor_after == cursor_before` is also
valid — a pure liveness ping. The server updates `updated_at` and
returns 200.

## Source events

A source event is a source-authored fact that may trigger a workflow.
Ox does not interpret the source's domain model. It stores the event,
matches triggers against its fields, and passes selected fields into the
new workflow execution.

The core envelope is:

| Field | Meaning |
|-------|---------|
| `source` | Watcher namespace, such as `cx`, `github`, `linear`, `schedule` |
| `kind` | Source-native event kind, such as `node.ready`, `issue.labeled`, `schedule.tick` |
| `subject_id` | Source-native correlation key for what the event is about |
| `tags` | Routing labels used by triggers |
| `data` | Free-form source payload available to trigger var templates |

Kinds are conventions, not a closed enum. A cx watcher might emit
`node.ready`, `comment.added`, or `phase.complete`. A GitHub watcher
might emit `issue.labeled`, `pull_request.opened`, or
`check_suite.failed`. A timer watcher might emit `schedule.tick`.
Neither the server nor the trigger matcher needs to understand these
strings beyond matching them.

## Trigger config becomes source-aware

```toml
[[trigger]]
on       = "node.ready"
source   = "cx"                      # new — filter by originating watcher
tag      = "workflow:code-task"
workflow = "code-task"
[trigger.vars]
branch = "cx-{event.subject_id}"
source_id = "{event.subject_id}"
title = "{event.data.title}"
```

`source` is an optional extra filter. A Linear project's trigger would
use `source = "linear"`; a watcher-agnostic trigger can omit it.

The trigger maps event fields into execution vars. Branch names are not
special in the core event model — they are workflow vars chosen by the
trigger and consumed by the workflow's workspace specs.

## Adaptive orchestration

Ox executions are deterministic. Ox systems are adaptive.

Adaptation happens through side effects and events, not through an agent
choosing arbitrary next steps inside a running workflow. A step may create
an issue, write a PRD, add a label, post a comment, merge code, or update
project state. Watchers observe those side effects and ingest them as
events. Triggers match those events and start new workflow executions.

Example:

1. A planning workflow writes a PRD node into cx.
2. The cx watcher observes `node.ready` with `workflow:breakdown-prd`.
3. Ox starts the deterministic `breakdown-prd` workflow.
4. That workflow creates implementation nodes.
5. The cx watcher observes those nodes becoming ready.
6. Ox starts `code-task` workflow executions.
7. `execution.completed` or a source-side state change can trigger an
   `assign-work` workflow that looks for the next work to unblock.

This keeps each execution inspectable and replayable while allowing the
larger system to coordinate itself through facts in the world.

## What moves out of ox-server

- `ox-server/src/cx.rs` — entire file relocates to a new crate
  `ox-cx-watcher`.
- `cx_poll_loop` in `ox-server/src/main.rs` — deleted.
- `CX_CURSOR_KEY` and its KV row — deleted. Cursor storage stays in
  the server, but generalizes into a `watcher_cursors` table keyed by
  watcher name; no cx-specific code or keys remain.
- `ox_core::events::EventType::Cx*` — collapsed into a single
  `SourceEvent` carrying `source`, `kind`, and `subject_id` fields.
- Shelling out to `cx` from server code — deleted.

The git HTTP endpoint (used by runners to clone workspaces) stays. The
runner-side coupling to cx (posting comments, claiming nodes, integrating
nodes from inside step prompts) is **out of scope for this change** —
see "Open Questions" below.

## Lifecycle / ox-ctl integration

`ox-ctl up` learns to launch watchers from project config:

```toml
# .ox/config.toml
watchers = ["cx"]
```

For ccstat (complex-native) the watchers list is `["cx"]`. For a
Linear-integrated project it would be `["linear"]`. Multiple watchers
are allowed; each runs as its own process. `ox-ctl status` surfaces
watcher health alongside runners and the herder: alive, last-event-at,
last-error.

Each watcher binary is a separate install target in the workspace
Makefile. Users who don't use cx don't need `ox-cx-watcher` on disk.

## Migration plan (vertical slices)

Each slice leaves the tree green and shippable.

1. **Ingest endpoint + cursor storage.** Add `watcher_cursors` table.
   Add `GET /api/watchers/{source}/cursor` and `POST
   /api/events/ingest` with batch + CAS semantics. Add `SourceEvent`
   event type. Unit-test the endpoint, dedup, and cursor CAS. At this
   point nothing calls it yet; the cx poller still runs in-process.

2. **Generic trigger matching.** Extend the trigger matcher to fire on
   `SourceEvent { source, kind, subject_id, tags }`. Unit-test against a
   constructed event.

3. **`ox-cx-watcher` binary.** New crate. Stateless on disk: fetches
   its cursor from the server on boot, runs `cx log` every 10s, posts
   batches to `/api/events/ingest` with `cursor_before` / `cursor_after`.
   End-to-end test with a temp repo + in-memory server.

4. **Switchover.** `ox-ctl up` launches `ox-cx-watcher`;
   `cx_poll_loop` in ox-server is deleted or disabled. Cx-specific
   trigger syntax is replaced with source/kind trigger syntax.

5. **Delete the old path.** `ox-server/src/cx.rs` removed. `Cx*`
   event types removed. Trigger syntax migration complete.

## Open questions

1. **Cursor cold-start semantics.** On first boot `GET
   /api/watchers/{source}/cursor` returns `null`. For replayable sources,
   the watcher snapshots current source state, emits events for currently
   actionable work, and starts its cursor at the current source head.
   Source-specific watchers decide what "actionable" means.

2. **Idempotency granularity.** Dedup via `(source, idempotency_key)` is
   the chosen boundary. Alternative: dedup by `(source, subject_id, kind)`
   tuple and let event seq ordering resolve conflicts. The explicit key
   is simpler and makes the watcher side clearer.

3. **Watcher health surfacing.** `ox-ctl status` currently shows
   runner and herder state. Watchers should appear there too. Since
   cursors live on the server, most of this is free: `watcher_cursors`
   already records `updated_at`, and last-error can be a column on the
   same table written on CAS failures or malformed batches.

4. **Multi-watcher-per-source.** Does it make sense to run two cx
   watchers at once? The `cursor_before` CAS resolves races cleanly:
   only one batch per round-trip wins, the loser refetches and tries
   again. This is effectively a Kafka consumer-group-of-one per
   source. No explicit support needed.

5. **Server unavailability.** Because checkpointing goes through the
   server, a server outage means the watcher can't advance its
   cursor. Watchers must buffer in-memory and retry; on extended
   outages they stop making progress until the server is back. This
   is a deliberate tradeoff against the old "local cursor file"
   design, where a watcher could keep checkpointing locally while the
   server was down. The win is that there is exactly one place to
   inspect, reset, or rewind a cursor — and no risk of the watcher's
   local file diverging from server-side reality.

6. **Replayable source requirement.** Stateless watchers are safe when
   the source can replay from the server-side cursor. Non-replayable
   webhook sources must rely on source-provided durable delivery or keep
   their own durable inbox before posting to Ox.

7. **Runner → external mutations.** ox-runner inside the VM shells out
   to `cx` to post comments, integrate nodes, etc. That coupling is
   orthogonal to event ingestion — it's a different abstraction (the
   runtime/step layer). Decoupling *ingestion* gets ox to "anything can
   trigger a workflow." Decoupling *mutation* gets ox to "any workflow can
   report back to any source." Both are useful, but they're separate
   phases. This document covers ingestion only.

## Non-goals

- Replacing cx. cx stays as the reference event source and the
  day-one happy path. ox-cx-watcher ships alongside ox-server.
- A webhook receiver. Watchers are push-side only, from watcher to
  server. A watcher that needs to receive webhooks (GitHub, Linear)
  runs its own HTTP listener; ox-server doesn't.
- Runtime plugin loading. Watchers are processes, not dynamic
  libraries. You install a watcher by installing a binary.
- A generic task model. Ox has events, workflow executions, steps,
  branches, and artifacts. Source systems may have tasks, issues, PRs,
  tickets, or schedules; Ox does not normalize them.
- Cross-watcher event ordering. Each watcher's event stream is
  independently ordered by its own cursor; ox-server's event bus
  assigns a total order on ingest but doesn't guarantee any
  causal relationship across watchers.
