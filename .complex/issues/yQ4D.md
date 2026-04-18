# Bug

After `ox-ctl up --runners=2` → `ox-ctl down` → `ox-ctl up --runners=1`, `ox-ctl status` shows 2 runners. Ghosts come from event replay: `ox-ctl down` SIGTERMs without draining, so no `RUNNER_DRAINED` events exist; on server restart the counter resets and event replay resurrects old `RUNNER_REGISTERED` entries.

## Plan

Two independent slices, both shipping the same fix from different angles.

### Slice A — `ox-ctl down` drains before kill
Fetch `/api/state/pool`, `POST /api/runners/:id/drain` for each non-drained runner, then SIGTERM. Best-effort: if server's already dead, skip drain. Unit-test a pure `runner_ids_to_drain(pool_json) -> Vec<String>` helper.

### Slice C — Server sweeps orphans on startup
After `EventBus::new` replays events, walk `PoolState.runners`. For each non-Drained runner with no heartbeat or heartbeat older than `grace_secs`, emit `RUNNER_DRAINED` with reason `"orphan at startup"`. Covers crashes, SIGKILLs, and anything that bypassed drain. Test: seed stale registration, run sweep, assert event appended.

### Out of scope
- Not making RunnerId stable across restarts (counter resets are orthogonal once ghosts are gone).
- Not removing drained runners from the projection — deliberate, keeps shutdown visibility in `ox-ctl status`.
