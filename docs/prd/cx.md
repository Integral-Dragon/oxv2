# cx Integration

cx is a file-native hierarchical issue tracker. It is a passive tool —
it stores the work graph and answers queries. It has no knowledge of ox.

ox depends on cx; cx does not depend on ox.

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

## git log as Event Source

cx has its own event log (`events/`). It is not used by ox.

Instead, ox-server derives cx events from the git log. Every cx mutation
is a commit that touches `.complex/`. When a branch merges to main,
ox-server diffs `.complex/` against the previous HEAD and emits cx events
based on what changed.

This approach:

- **Eliminates merge conflicts** — cx's `events/` files are append-only
  logs that conflict when multiple branches merge. git's object model
  handles JSON node files cleanly as long as different branches touch
  different nodes.
- **Removes redundancy** — git already records the full mutation history.
  A separate event log is a duplicate that can diverge.
- **Ties cx state to code state** — a cx mutation and its associated code
  change land on main atomically in the same merge commit.

### How ox-server processes cx changes

On every merge to main, ox-server runs:

```
git diff <previous_main> <new_main> -- .complex/nodes/
```

It parses the diff to determine what changed per node: state transitions,
new tags, new comments. It then emits the corresponding cx events into
the ox event log.

ox-server also maintains a cx projection (`GET /api/state/cx`) derived
from these events — a live mirror of the current state of `.complex/` on
main.

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

cx events are emitted by ox-server when a merge to main produces changes
to `.complex/`. Git events are emitted by ox-server when it performs or
attempts git operations.

All events follow the common envelope defined in [events.md](events.md).

### cx events

```
cx.task_ready       { node_id, tags[], workflow }
cx.task_claimed     { node_id, part }
cx.task_integrated  { node_id }
cx.task_shadowed    { node_id, reason }
cx.comment_added    { node_id, tag, author }
cx.phase_complete   { node_id }
```

`cx.task_ready` — a node transitioned to ready and carries a `workflow:X`
tag. The herder creates an execution for the matching workflow.

`cx.task_claimed` — a node transitioned to claimed. `part` is the agent
or persona that claimed it.

`cx.task_integrated` — a node transitioned to integrated.

`cx.task_shadowed` — a node was shadowed (blocked from further execution
after exhausting retries).

`cx.comment_added` — a comment was added to a node. `tag` is the comment
tag (e.g. `proposal`, `review`, `code-review`). Used to trigger workflows
that react to specific comment types.

`cx.phase_complete` — all children of a `#phase` node are integrated.
Triggers the `phase-review` workflow.

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
