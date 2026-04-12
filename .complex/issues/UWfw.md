# Plan: Config-driven trigger → workflow var mapping + ExecutionOrigin

## Context

Today every cx-triggered workflow in oxv2 silently assumes a workflow var named
`task_id` carries the cx node id. The herder and the `/api/triggers/evaluate`
endpoint hardcode this in 5 places: they stuff `vars["task_id"] = node_id` when
firing a trigger, and they dedup active executions by grepping `vars["task_id"]`.
A workflow that uses a different var name (e.g. `consultation` uses `branch`)
silently doesn't get the node id under the name it expects, and the dedup key
is meaningless for it.

This breaks the "config-driven" premise of ox: workflow files should declare
what they need, not be forced to use a magic var name. Two coupled fixes:

1. **Make trigger → workflow var plumbing explicit** via a `[trigger.vars]`
   block on each trigger, with `{event.*}` interpolation. The trigger author
   maps event fields into whatever var names the workflow declares.

2. **Decouple execution identity from workflow vars** with a typed
   `ExecutionOrigin` enum. Dedup keys on `(workflow, origin)`, not on a string
   match against `vars["task_id"]`. The `--task` filter on `ox-ctl exec list`
   goes away (`--status` and `--workflow` stay).

Outcome: workflows like `consultation` work correctly under cx triggers, the
five hardcoded `task_id` sites collapse, and the data model stops lying about
what an execution is.

---

## Target design (unchanged from prior plan)

### `[trigger.vars]` with `{event.*}` interpolation

```toml
[[trigger]]
on       = "cx.task_ready"
tag      = "workflow:code-task"
workflow = "code-task"
[trigger.vars]
task_id = "{event.node_id}"

[[trigger]]
on       = "cx.task_ready"
tag      = "workflow:consultation"
workflow = "consultation"
[trigger.vars]
branch = "{event.node_id}"
```

**v1 event field namespace** (cx events only — workflow chaining is data-model
ready but not wired through the herder):

| Event | Available fields |
|-------|------------------|
| `cx.task_ready` | `event.node_id` |
| `cx.task_claimed` | `event.node_id` |
| `cx.task_integrated` | `event.node_id` |
| `cx.task_shadowed` | `event.node_id`, `event.reason` |
| `cx.comment_added` | `event.node_id`, `event.tag`, `event.author` |

`event.tags` is omitted from v1 — no current workflow needs it.

### `ExecutionOrigin` enum

```rust
pub enum ExecutionOrigin {
    CxNode { node_id: String },
    Execution {
        parent_execution_id: ExecutionId,
        parent_step: Option<String>,
        kind: ChildKind,        // Escalated | Completed | StepCompleted | StepFailed
    },
    Manual { user: Option<String> },
}
```

- `PartialEq + Eq + Hash + Clone + Serialize + Deserialize`.
- Persisted on `execution.created`. On the wire: `Option<ExecutionOrigin>` with
  `#[serde(default)]`. In the projection it's resolved to non-optional via a
  fallback rule (see Slice B).

### Dedup

```rust
execs.values().any(|e|
    e.origin == origin
    && e.workflow == workflow_name
    && active(e.status)
)
```

`active()` is unified: API path blocks on `running` only; herder auto-eval
blocks on `running|escalated`.

### `trigger.failed` event

```rust
pub struct TriggerFailedData {
    pub source_seq: Seq,
    pub on: String,
    pub tag: Option<String>,
    pub workflow: String,
    pub reason: TriggerFailureReason,
}

pub enum TriggerFailureReason {
    MissingEventField { path: String },
    ValidationFailed  { message: String },
    UnknownWorkflow,
}
```

Emit-and-forget, replay-guarded.

### CLI

- Drop `--task` on `ox-ctl exec list`
- Keep `--status` and `--workflow`
- Add an `ORIGIN` column, 24 chars, ellipsis. Format:
  `cx:{node_id}` / `exec:{short8}/{step}` / `manual:{user?}`

---

## Vertical slices

Each slice is an independent red/green cycle that leaves the tree working and
delivers an observable capability. Slices are sequenced so that each one
justifies its own existence without relying on the next, and so that an
observer looking only at the commit history can point at a behavior that
didn't exist before.

### Slice A — `[trigger.vars]` works end-to-end for a cx trigger

**User-visible outcome:** a trigger in `triggers.toml` with a `[trigger.vars]`
block fires a real cx event into a real workflow with the mapped vars, for
both `code-task` (var named `task_id`) and `consultation` (var named `branch`).

**Red test** (integration-style, in `ox-herder` or a new top-level
`tests/trigger_mapping.rs`):

- Boot a minimal herder + in-process server with the two default triggers
  loaded, each with `[trigger.vars]` set per the target design.
- Inject a `cx.task_ready` event for a node tagged `workflow:consultation`.
- Assert the resulting `execution.created` event carries
  `vars["branch"] = <node_id>` and does NOT carry `vars["task_id"]`.
- Assert the same for `workflow:code-task` (with `task_id` populated).

The test will fail today because the herder ignores `[trigger.vars]` and
hardcodes `task_id`.

**Green — minimum to pass:**

1. Add `pub vars: HashMap<String, String>` to `TriggerDef` with
   `#[serde(default)]`. (workflow.rs:202)
2. Add a minimal `EventContext` enum in `ox-core/src/events.rs` with only the
   variants the test needs (`CxTaskReady { node_id }` plus whatever the herder
   already has for the other cx.* handlers — stub them with unused fields for
   now, wire only `CxTaskReady`).
3. `TriggerDef::build_vars(&self, ctx: &EventContext) -> Result<HashMap<String,String>, TriggerError>`
   — dumb `{event.X}` interpolator. Fail on missing field; return the map
   otherwise.
4. Herder: in `evaluate_triggers_for_node` (herder.rs:~920–950), replace the
   hardcoded `trigger_vars.insert("task_id", ...)` block with
   `trigger.build_vars(&ctx)`. Same for the api.rs `evaluate_triggers` site
   (api.rs:~1065).
5. Update `defaults/workflows/triggers.toml` to add `[trigger.vars]` to both
   triggers.

**Deliberately out of this slice:** `ExecutionOrigin`, the `trigger.failed`
event, the `--task` CLI removal. Dedup stays on `vars["task_id"]` for code-task
(still works because code-task still populates it via the new path). For
consultation, dedup is technically broken but the test asserts a single fire,
so we don't hit it.

**Commit shape:** one red commit, one green commit.

---

### Slice B — dedup works for any workflow regardless of var name

**User-visible outcome:** firing `cx.task_ready` twice for the same
consultation node produces exactly one execution. Fixing the subtle dedup bug
for workflows that don't use `task_id`.

**Red test:**

- Same test harness as Slice A.
- Inject the same `cx.task_ready` for a `workflow:consultation` node twice
  in quick succession.
- Assert exactly one `execution.created` event lands.

Will fail today and after Slice A — the dedup check at herder.rs:910 and
api.rs:1050 greps `vars["task_id"]`, which isn't set for consultation, so the
second trigger falls through and creates a duplicate execution.

**Green — minimum to pass:**

1. Add `ExecutionOrigin`, `ChildKind` to `ox-core/src/events.rs`.
2. Add `origin: Option<ExecutionOrigin>` to `ExecutionCreatedData` with
   `#[serde(default)]`.
3. Add `origin: ExecutionOrigin` to `ExecutionState` (projections.rs:44) and a
   `fallback_origin(vars)` helper implementing the synth rule:
   `if vars["task_id"] → CxNode{} else → Manual{}`.
4. Wire the projection's `ExecutionCreated` arm (projections.rs:~353) to use
   `data.origin.unwrap_or_else(|| fallback_origin(&data.vars))`.
5. Replace the 5 hardcoded `vars["task_id"]` dedup/filter sites with origin
   equality compares:
   - herder.rs:910 (cx trigger dedup) → origin compare
   - herder.rs:945 (hardcoded insert) — already gone from Slice A, verify
   - api.rs:346 (`--task` filter) → leave filter working but implemented via
     origin (temporary; removed in Slice D)
   - api.rs:1050 (manual trigger dedup) → origin compare
   - api.rs:1065 (hardcoded insert) — already gone from Slice A, verify
6. Unify `active()` helper: API uses `running` only; herder uses
   `running|escalated`. Single function, two callers.
7. When creating an execution from a cx trigger, pass
   `Some(CxNode { node_id })` through `CreateExecutionRequest` →
   `ExecutionCreatedData.origin`. Add the field to `CreateExecutionRequest` in
   `ox-core/src/client.rs`.
8. `create_execution` in api.rs defaults to `Some(Manual { user: None })` when
   no origin supplied.

**Commit shape:** one red commit, one green commit. May iterate — if the
dedup comparison exposes that the projection order matters (event-ordering
edge case), go back to red.

---

### Slice C — bad `[trigger.vars]` surfaces as a `trigger.failed` event

**User-visible outcome:** a typo like `task_id = "{event.bogus}"` in the
trigger file no longer silently drops the trigger — it produces a
`trigger.failed` event visible in the event log and SSE stream.

**Red test:**

- Load a trigger with `[trigger.vars]` referencing a bogus event field.
- Inject a matching cx event.
- Assert a `trigger.failed` event appears in the log with
  `reason: MissingEventField { path: "bogus" }`.
- Assert NO `execution.created` event follows.
- Second test: vars map that fails `validate_vars` → `ValidationFailed`
  reason.

**Green — minimum to pass:**

1. Add `TriggerFailedData`, `TriggerFailureReason`, `EventType::TriggerFailed`
   to `ox-core/src/events.rs`.
2. `TriggerError` enum returned by `build_vars` (from Slice A — may have been
   a placeholder; flesh out here).
3. Herder and api.rs trigger evaluation paths: on `Err` from `build_vars` or
   `validate_vars`, append a `trigger.failed` event and `continue` the loop
   instead of creating an execution.
4. Herder emission guarded by `if !self.replaying` (herder.rs pattern used for
   other cx.* arms).
5. New client helper `post_trigger_failed(...)` in `ox-core/src/client.rs`
   used by the herder.

**Commit shape:** one red, one green. Possibly a second red/green pair for
the `ValidationFailed` branch if the first slice surfaces that they share
enough machinery to warrant separating.

---

### Slice D — CLI shows origin, `--task` is gone

**User-visible outcome:** `ox-ctl exec list` renders an `ORIGIN` column; the
`--task` flag is removed; `--status` and `--workflow` still work.

**Red test:**

- Snapshot-style test of `cmd_exec_list` output rendering with a fixture set
  of executions across all origin variants. Asserts the column, width, and
  formatting.
- A test that `clap` rejects `--task` as an unknown flag.
- An assertion that `GET /api/executions?task=X` returns 400 (or silently
  ignores, depending on serde defaults — pick the stricter behavior).

**Green — minimum to pass:**

1. Delete `task: Option<String>` from `ExecCommands::List` (main.rs:88).
2. Drop `_task` param through the call stack (main.rs:179, 243).
3. Delete the `task` field on `ListExecutionsQuery` in api.rs and the
   corresponding filter line (api.rs:346 — the origin-based version from
   Slice B goes too, since there's no flag to filter on anymore).
4. Add `format_origin(&ExecutionOrigin) -> String` with the 24-char ellipsis
   rule.
5. Insert the ORIGIN column into the table header and row rendering
   (main.rs:256–291).
6. Include `origin` in the `execution_summary` JSON output (api.rs:~380) and
   in `ExecutionDetail` (client.rs:39).
7. Update `cmd_exec_show` to render the origin field.

**Commit shape:** one red, one green.

---

### Slice E — docs match reality

**User-visible outcome:** the PRD and design docs describe the new trigger
and origin model. This slice has no code test — the test is "a reader of
the docs can write a correct `[trigger.vars]` block without reading the
source."

**Verification:**

- `docs/prd/workflows.md` — new section "Trigger variable mapping" under
  "Triggers" with the `[trigger.vars]` syntax, the `event.*` field table
  for v1, the v1 cx-only scope.
- `docs/prd/events.md` — add `trigger.*` to the namespace table.
- `docs/workflows-design.md` — rewrite "Trigger Evaluation" section to show
  the `EventContext` build step and the failure path.
- `docs/api.md` — drop `?task=` from `GET /api/executions`, document `origin`
  on `POST /api/executions`, document `trigger.failed` event shape.

**Commit shape:** one docs commit. No red phase — docs aren't TDD.

---

## Slice dependencies

```
A (trigger.vars) ──▶ B (origin dedup) ──▶ C (trigger.failed)
                                         ╲
                                          ╲
                          D (CLI) ◀───────┘
                                          ╲
                                           ╲
                                            E (docs)
```

C depends on A (needs `TriggerError`) and loosely on B (loop structure is
cleaner post-B). D depends on B (needs origin formatting). E can happen any
time after D.

## Deferred to v2

- Workflow chaining triggers (`on = "execution.escalated"` etc). Data model
  variants exist from Slice B; the herder arms are not wired.
- `event.tags` and any path syntax.
- `cx.task_ready` `event.state` field.
- `ox-ctl exec tree` and ancestry queries.
- Auth-aware `Manual { user: Some(_) }`.

## Critical files

- `ox-core/src/events.rs`
- `ox-core/src/workflow.rs`
- `ox-core/src/client.rs`
- `ox-server/src/projections.rs`
- `ox-server/src/api.rs`
- `ox-herder/src/herder.rs`
- `ox-ctl/src/main.rs`
- `defaults/workflows/triggers.toml`
- `docs/prd/workflows.md`, `docs/prd/events.md`, `docs/workflows-design.md`, `docs/api.md`

## Cross-cutting verification (after all slices)

- `cargo build --workspace`
- `cargo clippy --workspace -- -D warnings`
- `cargo test --workspace`
- Manual: start ox-server + ox-herder + ox-runner against a fresh repo, tag a
  cx node `workflow:consultation`, confirm the branch is named after the node
  id and the execution's `origin` is `CxNode`. Repeat for `workflow:code-task`.
- Manual: break a `[trigger.vars]` template, reload, fire a cx event, confirm
  `trigger.failed` in the event log and no execution.
- Manual: restart ox-server mid-run; replay reconstructs origins for both new
  and legacy (`task_id`-synthesized) events without crashing.
