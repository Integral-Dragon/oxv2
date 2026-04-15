**Capability.** A trigger declared with `source = \"cx\"`, `on = \"node.ready\"` fires a workflow when a matching `EventType::Source` arrives. Trigger vars resolve from `event.source`, `event.kind`, `event.subject_id`, `event.data.*`.

**PRD edits:**
- `docs/prd/workflows.md` — rewrite trigger section around `source`/`on`/`tag`. Replace `### cx triggers` with `### Source event triggers`. Replace `{task_id}` examples with `{event.subject_id}` / `{branch}`. Rename "inter-step communication via cx comments" to "artifacts, branch state, and source side effects — cx comments are one source-specific example." `merge_to_main` description: "merge the execution branch" not "merge the task branch."
- `docs/prd/execution.md` — "workflow for a task" → "workflow execution created from a trigger." "Checks out the task branch" → "checks out the execution branch." "Shadow the cx task" → "escalate the execution; source-specific workflows may mark source objects shadowed." Example triggers use `source`/`kind` syntax.

**Design edits:**
- `docs/workflows-design.md` — rewrite trigger evaluation around `SourceEventData { source, kind, subject_id, tags }`. Replace `CxNode` origin with `Source { source, kind, subject_id }`. Add note: state-suppression (integrated/shadowed filter) moves from herder into the cx watcher.

**Code (red/green). Red — unit test in `ox-herder/src/herder.rs`:**
Build a synthetic `EventEnvelope { EventType::Source, data: SourceEventData { source: \"cx\", kind: \"node.ready\", subject_id: \"Q6cY\", tags: [\"workflow:code-task\"], ... } }`. Pass through `evaluate_triggers_for_node_with_state()`. Assert a matching trigger fires an execution with the right resolved vars.

**Green:**
- Add `EventContext::Source` + `resolve()` walk into `event.data.*`.
- Add `source: Option<String>` field to `TriggerDef`.
- Add `ExecutionOrigin::Source { source, kind, subject_id }`.
- Extend matcher to dispatch `EventType::Source` envelopes.
- Update ccstat's `triggers.toml` to `source = \"cx\", on = \"node.ready\"` syntax.

Legacy `CxNode` origin + `Cx*` matcher path stays in place. Slice 5 deletes them.

Depends on: Slice 1.