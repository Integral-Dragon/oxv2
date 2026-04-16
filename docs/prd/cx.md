# cx: The Reference Event Source

cx is a file-native hierarchical issue tracker. It is a passive tool —
it stores the work graph and answers queries. It has no knowledge of ox.

ox depends on cx only through **`ox-cx-watcher`**, a standalone
process that observes a cx-enabled repository and posts source events
to ox-server's ingest endpoint. cx is not wired into ox-server at all;
swapping cx for Linear, GitHub Issues, or anything else is a matter of
replacing the watcher binary, not changing the server.

This document covers the cx integration specifically. For the
watcher-plugin boundary the server exposes, see
[event-sources.md](event-sources.md) and
[../event-sources-design.md](../event-sources-design.md).

---

## What cx Provides

A cx installation is a `.complex/` directory in the repository. It
contains:

- **Nodes** — the work graph. Each node has an ID, title, state, tags,
  optional body, comments, edges, and a `meta` field for arbitrary
  orchestrator data.
- **Node states** — `latent → ready → claimed → integrated`
- **Edge types** — `blocks`, `waits-for`, `discovered-from`, `related`
- **Tags** — free-form strings used for filtering and trigger matching
- **Comments** — timestamped, authored, optionally tagged entries on a
  node. Used for inter-step communication (proposals, reviews, verdicts)
- **meta** — arbitrary JSON on a node. cx ignores it. ox uses it to store
  workflow hints, retry counts, and execution references

See the cx documentation for the full data model and CLI reference.

---

## The Single Writer Rule

ox-server is the only process that writes cx to main. This is enforced
by the branch discipline:

- Agents write cx freely on their branch — `cx claim`, `cx comment`,
  `cx integrate`, etc. This is expected and correct.
- The user writes cx through interactive workflow steps on a branch.
- No actor writes cx directly to main.
- ox-server's `merge_to_main` action is the only path for cx changes to
  reach main.

The merge serialises all cx mutations. There are no concurrent writers
to main's `.complex/`.

---

## ox-cx-watcher

`ox-cx-watcher` is a standalone binary. `ox-ctl up` launches it
alongside `ox-server` and `ox-herder` when `.ox/config.toml` lists
`cx` in its `watchers = [...]` array. A machine running ox without cx
simply leaves the watcher off the list; no `ox-*watcher` binary needs
to be on disk.

### What the watcher does

Every tick (10s by default):

1. Call `GET /api/watchers/cx/cursor` to read the last committed
   cursor. On first boot the server returns `cursor: null` — the
   cold-start case described below.
2. Run `cx log --json --since <cursor>` in the repo. The latest
   commit SHA becomes the next cursor.
3. For every touched node, run `cx show <id> --json` to get the
   current canonical state, then map that snapshot into an
   `IngestEventData` record:

   | cx state | event kind |
   |----------|------------|
   | `ready` | `node.ready` |
   | `claimed` | `node.claimed` |
   | `integrated` | `node.done` |
   | `latent` | *(no event)* |

   Each record carries `subject_id = <node_id>`, the node's tags, and
   the full node snapshot JSON as `data`. The idempotency key is
   `<node_id>:<kind>:<short_sha>` so the same state transition
   observed in multiple ticks dedups server-side. `source = "cx"` is
   set once on the enclosing `IngestBatch`.
4. New comments are mapped into `kind = "comment.added"` events with
   a stable idempotency key based on the comment author, tag, and
   parent node SHA.
5. POST one `IngestBatch` to `/api/events/ingest` with
   `cursor_before = <old cursor>` and `cursor_after = <new cursor>`.
6. On 409 (CAS mismatch — another watcher wrote to this source, or
   the row was manually reset), re-GET the cursor and retry with the
   fresh value.
7. On 5xx/network error, back off and retry with the same batch. The
   watcher never advances its in-memory cursor until the server has
   returned 200.

### Cold start

On the very first call, `GET /api/watchers/cx/cursor` returns `null`.
The watcher does not replay the full cx log — that could mean
thousands of historical commits. Instead it:

1. Snapshots the current `git rev-parse HEAD` as the cursor-to-be.
2. Fetches `cx list --json` and emits one `node.ready` /
   `node.claimed` / `node.done` event per currently actionable node.
3. POSTs a single batch with `cursor_before: null, cursor_after:
   <HEAD>`.

This means bringing ox up against a repo with existing cx state
immediately produces source events for whatever work is currently
ready, without replaying history.

### Why the watcher is stateless on disk

The cursor lives on the server inside the same transaction as the
event append and idempotency record. The watcher holds only an
in-memory copy of the last-committed cursor. If the watcher crashes
it re-fetches the cursor on boot; if the server was down during the
last tick the watcher's in-memory state is lost but the server
cursor never advanced, so the next boot re-derives the same batch
from cx.

This keeps there being one place to inspect, reset, or rewind the cx
cursor — the `watcher_cursors` row — and avoids the class of bugs
where a watcher's local cursor file diverges from server reality.

### State suppression lives in the watcher

Integrated and shadowed nodes must not start new workflow executions
through an auto-fired `node.ready` event. The filter that enforces
this lives inside `ox-cx-watcher/mapping.rs` — the watcher simply
does not emit `node.ready` for a node it observes as already
`integrated` or `shadowed`. The server-side trigger matcher has no
cx-specific logic; it matches what the watcher chose to ingest.

### Post-merge side effects

ox-server's `merge_to_main` action does not try to emit cx events
directly. After a merge lands, the cx watcher observes the new
commits on its next tick (or sooner if triggered by a merge signal
in a future revision) and emits the corresponding source events
through the same path as everything else. There is one code path for
cx events, not two.

---

## Branch Discipline

All cx mutations travel with the branch they were made on. This is what
makes the single-writer rule practical:

1. An agent clones the repo and checks out the task branch
2. It makes cx mutations freely — claim, comment, integrate
3. Those mutations are committed to `.complex/` on the branch
4. When the workflow's `merge_to_main` step runs, the branch merges to
   main — bringing the code and the cx state together atomically
5. ox-server processes the merge, derives cx events, updates its
   projection

The cx state on main always reflects completed, merged work. In-progress
cx changes are on branches, invisible to ox-server's event processing
until they merge.

---

## Workflow Tags

cx tags are the primary mechanism for connecting the work graph to the
workflow engine. A node tagged `workflow:code-task` in a `ready` state
triggers the `code-task` workflow.

Tag conventions used by ox:

| Tag pattern | Meaning |
|-------------|---------|
| `workflow:<name>` | Run this workflow when the node becomes ready |
| `plan` | This node is a plan root |
| `phase` | This node is an execution phase |
| `objective` | This node is a product objective |
| `bug` | Unplanned bug report — triggers triage |
| `opportunity` | Unplanned opportunity — triggers assessment |

Tags are inherited from ancestors at read time in cx. A task under a
`#phase` node effectively carries the `phase` tag without needing it
explicitly.

---

## meta Field

The `meta` field on cx nodes is arbitrary JSON that ox writes and cx
ignores. ox uses it to annotate nodes with execution context:

```json
{
  "execution_id": "aJuO-e1",
  "workflow": "code-task",
  "attempts": 2,
  "last_step": "review-code"
}
```

Agents can read `meta` via `cx show --json` to understand the execution
context of a task they have been assigned.

---

## Events

All cx facts reach ox-server as canonical event envelopes authored
by `ox-cx-watcher` and stamped with `source = "cx"`. Triggers match
on `source = "cx"` and the watcher-native kinds listed below.

All events follow the common envelope defined in [events.md](events.md).

### Source event kinds emitted by ox-cx-watcher

```
{ source: "cx", kind: "node.ready",    subject_id: <node_id>, tags, data }
{ source: "cx", kind: "node.claimed",  subject_id: <node_id>, tags, data }
{ source: "cx", kind: "node.done",     subject_id: <node_id>, tags, data }
{ source: "cx", kind: "comment.added", subject_id: <node_id>, tags, data }
```

`node.ready` — a node transitioned to ready and is not shadowed. The
watcher filters out already-integrated and shadowed nodes before
emitting. Trigger with `source = "cx", on = "node.ready"`.

`node.claimed` — a node transitioned to claimed. Informational in
most workflows; used by some retro/observer workflows.

`node.done` — a node transitioned to integrated. Consumed by
workflows that react to completion (phase rollups, assign-next-work,
etc.).

`comment.added` — a comment was added to a node. `data.tag` is the
comment tag (e.g. `proposal`, `review`, `code-review`). Used to
trigger workflows that react to specific comment types.

Node snapshot JSON (state, tags, title, body, meta, edges) rides in
the event's `data` field, so triggers can template
`{event.data.title}` or `{event.data.state}` into workflow vars
without another round-trip to cx.

### git events

```
git.branch_pushed   { branch, sha, execution_id, step }
git.merged          { branch, into, sha, execution_id }
git.merge_failed    { branch, into, reason, execution_id }
```

`git.branch_pushed` — a runner has pushed a branch to ox-server's git
endpoint after completing a step.

`git.merged` — ox-server has merged a branch to main. Immediately
followed by any cx events derived from the merge diff.

`git.merge_failed` — the merge could not be completed. `reason` carries
the cause: `conflicts`, `empty_branch`, or `dirty_worktree`. The
workflow's `on_fail` handler runs.
