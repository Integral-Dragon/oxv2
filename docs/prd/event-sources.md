# Event Sources (Watcher Plugins)

> **Status:** Proposal. Not implemented. The current ox-server has a
> cx-specific poller (`ox-server/src/cx.rs` + `cx_poll_loop` in
> `main.rs`) baked in. This document describes the plan to decouple it.

## Problem

Today ox-server is hardcoded to one external system: **complex** (cx).
A `cx_poll_loop` runs inside ox-server, shells out to `cx log` every
10 seconds, parses the output, and appends cx-specific event types
(`CxTaskReady`, `CxTaskClaimed`, `CxTaskIntegrated`, `CxCommentAdded`)
to the event bus. Triggers match on these event types to fire
workflows.

This couples ox-server to cx. To run ox against a Linear project, a
GitHub repo's issues, Jira, or any other work tracker, ox-server would
need a second hardcoded poller — and a third, and a fourth. Every new
source is a change to ox-server itself.

The engine is otherwise source-agnostic: runner dispatch, pool
management, retries, reviews, merges, artifacts, metrics — none of it
cares where the triggering event came from. Only the ingestion path is
coupled.

## Goal

Make event ingestion a plugin boundary. ox-server stops polling
anything; external **watcher** processes own the integration with each
source system and push events into ox-server via a single HTTP
endpoint. Anyone can write a watcher in any language; ox-server has
zero knowledge of cx, Linear, GitHub, or anything else.

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
└───────────────┘                               └──────┬───────┘
  (one process                                         │
   per source,                                         ▼
   each owns                                      triggers →
   its cursor)                                    workflows
```

Each watcher:

- Is a standalone process, launched alongside ox-server the same way
  ox-herder is.
- Talks to exactly one source system. It knows that system's API
  (polling, webhooks, subscription streams — whatever's appropriate).
- Owns its own cursor / subscription state in its own state file,
  independent of ox-server's database.
- Maps source-native events onto a small set of generic event types
  (see below) and posts them to ox-server's ingest endpoint.
- Is otherwise invisible to ox-server. The server never calls a
  watcher; communication is one-way.

## API

ox-server exposes exactly one new endpoint:

```
POST /api/events/ingest
```

Body:

```json
{
  "source": "cx",
  "event_type": "task.ready",
  "external_id": "Q6cY",
  "idempotency_key": "cx:Q6cY:task.ready:d59b010",
  "tags": ["workflow:code-task"],
  "data": {
    "title": "ccstat models — model-mix breakdown over time",
    "node_id": "Q6cY",
    "state": "ready"
  }
}
```

Fields:

| Field             | Required | Purpose                                        |
|-------------------|----------|------------------------------------------------|
| `source`          | yes      | Watcher identifier (`cx`, `linear`, `github`)  |
| `event_type`      | yes      | One of the generic types in the table below    |
| `external_id`     | yes      | Source-native ID of the entity (for joining)   |
| `idempotency_key` | yes      | Dedup key; server rejects duplicates silently  |
| `tags`            | no       | Routing labels for trigger matching             |
| `data`            | no       | Free-form payload, templatable from workflows   |

On receipt, ox-server:

1. Dedups on `idempotency_key`. Duplicates return 200 with no side
   effects. (The watcher can retry freely on network failure.)
2. Appends the event to the bus as an `ExternalEvent`.
3. Runs trigger matching against loaded triggers.
4. Fires any matching workflow executions, same code path as today.

## Generic event types

Watchers collapse their source-native vocabulary onto a small set:

| Type              | Meaning                                      |
|-------------------|----------------------------------------------|
| `task.ready`      | Work item is ready to be picked up           |
| `task.claimed`    | Someone/something started work on it         |
| `task.done`       | Work item is integrated / closed             |
| `comment.added`   | New comment / note on a work item            |

Anything weirder goes into `data` and is picked out by workflow
templates. Resist adding new top-level types (`pr.opened`,
`build.failed`, etc.) until a real watcher needs one — tag-based
routing handles most cases.

Source-specific context stays in `data`. A GitHub watcher's
`task.ready` for a PR might put `pr_number`, `base_branch`, and
`head_sha` in `data`; a Linear watcher's `task.ready` might put
`cycle_id` and `priority`. Neither the server nor the trigger matcher
cares.

## Trigger config becomes source-aware

```toml
[[trigger]]
on       = "task.ready"
source   = "cx"                      # new — filter by originating watcher
tag      = "workflow:code-task"
workflow = "code-task"
[trigger.vars]
task_id = "{event.external_id}"
```

`source` is an optional extra filter. A Linear project's trigger would
use `source = "linear"`; a watcher-agnostic trigger can omit it.

Existing trigger syntax (`on = "cx.task_ready"`) is treated as a
deprecated alias during the migration and removed once all known
triggers are updated.

## What moves out of ox-server

- `ox-server/src/cx.rs` — entire file relocates to a new crate
  `ox-cx-watcher`.
- `cx_poll_loop` in `ox-server/src/main.rs` — deleted.
- `CX_CURSOR_KEY` and its KV in the server database — deleted. The
  watcher owns its own cursor state on disk.
- `ox_core::events::EventType::Cx*` — collapsed into a single
  `ExternalEvent` carrying `source` + `event_type` fields.
- Shelling out to `cx` from server code — deleted.

The git HTTP endpoint (used by runners to clone workspaces) stays. The
runner-side coupling to cx (posting comments, claiming tasks from
inside step prompts) is **out of scope for this change** — see "Open
Questions" below.

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

1. **Ingest endpoint.** Add `POST /api/events/ingest` with idempotency
   dedup. Add `ExternalEvent` event type. Unit-test the endpoint and
   dedup. At this point nothing calls it yet; the cx poller still
   runs in-process.

2. **Generic trigger matching.** Extend the trigger matcher to fire on
   `ExternalEvent { source, event_type, tags }`. Unit-test against a
   constructed event. Cx-typed triggers still work.

3. **`ox-cx-watcher` binary.** New crate. Owns a cursor file, runs
   `cx log` every 10s, posts to `/api/events/ingest`. End-to-end test
   with a temp repo + in-memory server. The in-server cx poller still
   runs — two paths in parallel, belt and braces.

4. **Switchover.** `ox-ctl up` launches `ox-cx-watcher`;
   `cx_poll_loop` in ox-server becomes a no-op behind a feature flag,
   then is deleted. Cx-specific trigger syntax starts warning.

5. **Delete the old path.** `ox-server/src/cx.rs` removed. `Cx*`
   event types removed. Trigger syntax migration complete.

## Open questions

1. **Cursor cold-start semantics.** If a watcher boots with no cursor
   file (first run, or the file was lost), does it replay everything
   (flood) or snapshot current state and start from HEAD (skip)? The
   existing cx poller snapshots on first boot; that's probably the
   right default, but it's a decision each watcher makes.

2. **Idempotency granularity.** Dedup via `idempotency_key` is the
   chosen boundary. Alternative: dedup by `(source, external_id,
   event_type)` tuple and let the event seq ordering resolve conflicts.
   The explicit key is simpler and makes the watcher side clearer.

3. **Watcher health surfacing.** `ox-ctl status` currently shows
   runner and herder state. Watchers should appear there too:
   last-successful-post, last-error, cursor position. This is a
   real UX feature, not just wiring.

4. **Multi-watcher-per-source.** Does it make sense to run two cx
   watchers at once? The API doesn't forbid it — idempotency-keys
   dedup races on the server. Design fails open. No explicit support
   needed.

5. **Runner → external mutations.** ox-runner inside the VM shells out
   to `cx` to post comments, integrate tasks, etc. That coupling is
   orthogonal to event ingestion — it's a different abstraction (the
   runtime/step layer). Decoupling *ingestion* gets ox to "anyone can
   trigger a workflow." Decoupling *mutation* gets ox to "anyone can
   build a workflow that reports back." Both are needed for a true
   plugin story, but they're separate phases. This document covers
   ingestion only; mutation is a follow-up PRD.

6. **Trigger backwards compatibility.** `on = "cx.task_ready"` in the
   wild needs to migrate to `on = "task.ready", source = "cx"`. For
   ccstat that's one file; for any downstream users it's a breaking
   change. Keep the old syntax as a deprecated alias for one release
   cycle, then remove.

## Non-goals

- Replacing cx. cx stays as the reference event source and the
  day-one happy path. ox-cx-watcher ships alongside ox-server.
- A webhook receiver. Watchers are push-side only, from watcher to
  server. A watcher that needs to receive webhooks (GitHub, Linear)
  runs its own HTTP listener; ox-server doesn't.
- Runtime plugin loading. Watchers are processes, not dynamic
  libraries. You install a watcher by installing a binary.
- Cross-watcher event ordering. Each watcher's event stream is
  independently ordered by its own cursor; ox-server's event bus
  assigns a total order on ingest but doesn't guarantee any
  causal relationship across watchers.
