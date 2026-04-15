**Capability.** Old cx-in-server code is gone. Every remaining doc that used task/cx/node as core language is cleaned up. The grep test passes.

**PRD edits:**
- `docs/prd/self-improvement.md` — phase/event/execution vocabulary replaces task. `{task_id}` branch examples → `{event.subject_id}` / `{workflow.phase_id}`. Retro prompt: "cx-diff: what task state changes?" → "source-diff: what source-specific side effects did this execution produce?" (cx-diff stays as the cx reference artifact). "Tokens per task" → "tokens per execution."
- `docs/prd/artifacts.md` — generalise to source side-effect artifacts. `cx-diff` stays as the cx reference artifact but not as a universal every-step artifact.
- `docs/prd/metrics.md` — `cx_nodes_created` and friends move to a cx-specific derived-metrics subsection. Core metrics use execution/workflow terminology.
- `docs/protocols.md` — `## Task` / `## Task Context` → `## Work` / `## Execution Context`. `{task_id}`, `{task_title}`, `{task_body}` → workflow vars supplied by the trigger. Source-specific prompt context injected by workflow/runtime config, not core prompt assembly. Keep cx examples under a "cx reference workflow" callout.

**Design edits:**
- `docs/design.md` — remove `cx` module from ox-server list; remove `Cx*` variants from the event enum; `watcher_cursors` + `ingest_idempotency` become fully canonical.
- `docs/api.md` — remove `origin: { type: \"cx_node\" }`, `GET /api/state/cx`, `trigger: \"cx.task_ready\"` from the main surface.
- `docs/storage.md` — delete `CxState`; the "deprecated" marker comes off the new tables.
- `docs/git-integration.md` — stop saying "ox-server polls cx"; describe cx watcher as a separate process.
- `docs/vm-layout.md` — confirm cx is one installed reference tool, not core.

**Code (red/green). Red:**
- Test that greps `ox-server/src` for `\\bcx\\b` and expects only comments. Existing tests still pass.

**Green:**
- Delete `ox-server/src/cx.rs`, `cx_poll_loop`, `CX_CURSOR_KEY` and its KV row.
- Delete `EventType::Cx*` + `*Data` structs.
- Delete `ExecutionOrigin::CxNode`.
- One-time event-log reset + cx watcher resync (user approved: no backwards compatibility, reset fine).
- Update ccstat's `triggers.toml` to drop any remaining `cx.task_ready` syntax.

Depends on: Slice 4.