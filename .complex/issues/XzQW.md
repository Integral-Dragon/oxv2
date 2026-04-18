# Plan

`ox-ctl status` currently prints pool aggregates only. Extend it to show a per-runner "runners" section with status + what each non-idle runner is working on: workflow name, execution id, step, attempt.

## Approach — client-side composition (no server change)

Server already exposes:
- `GET /api/state/pool` → runner list with `current_step: "exec_id/step/attempt"`
- `GET /api/executions/:id` → workflow name

ox-ctl joins these client-side.

## Slices

- Slice 1 (red/green): `parse_step_attempt("exec/step/attempt") -> Option<(exec, step, u32)>` + unit tests.
- Slice 2 (red/green): `format_runners_section(rows: &[RunnerRow]) -> String` + unit tests for idle / executing / mixed. Mirrors `format_watchers_section`.
- Slice 3: wire into `cmd_status()` — fetch pool state, fetch exec-by-id for unique exec ids, build rows, render. JSON mode adds a `runners` array. Manual verification by running an ensemble.
