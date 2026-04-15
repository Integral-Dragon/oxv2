**Capability.** `ox-ctl up` spawns `ox-cx-watcher` from `.ox/config.toml` `watchers = [...]` list. `cx_poll_loop` in ox-server is disabled (code still present). `ox-ctl status` shows watcher health from `GET /api/watchers`. System runs end-to-end on the new path.

**PRD edits:**
- `docs/prd/ox-ctl.md` — `--task` → `--subject`; `TASK` column → `ORIGIN`; `ox-ctl trigger <node-id>` becomes the generic form `ox-ctl trigger --source cx --kind node.ready --subject Q6cY`. Event filter examples use source/kind. Add a **Watchers** section to status output (alive, last-ingest-at, cursor, last-error).
- `docs/prd/platform.md` — "Task board — the cx work graph" → "Work graph (source-specific; cx is the local reference)." Onboarding reframed around source events.

**Design edits:**
- `docs/design.md` — update process-lifecycle: ox-ctl spawns server, herder, runners, **and** configured watchers. Remove "starts cx poll loop" from server startup; mark cx module entry as "(disabled — deleted in slice 5)".

**Code (red/green). Red — integration tests:**
1. `ox-ctl up` in a fixture repo with `watchers = [\"cx\"]`: `ox-cx-watcher` appears in the pidfile, server `cx_poll_loop` is not running, a fresh cx node still produces a workflow execution end-to-end.
2. `ox-ctl status` returns a Watchers section with the cx watcher marked alive.

**Green:**
- Add `watchers: Vec<String>` to `OxConfig` in `ox-core/src/config.rs`.
- Default ccstat config to `[\"cx\"]`.
- `ox-ctl/src/up.rs` spawns watchers after the herder; writes pidfile entries.
- `bins.watcher(name)` lookup resolves `ox-<name>-watcher` in the same dir as `ox-server`.
- Gate `cx_poll_loop` behind a flag defaulted off.
- `ox-ctl status` reads `GET /api/watchers` and renders the Watchers section.

Depends on: Slice 3.