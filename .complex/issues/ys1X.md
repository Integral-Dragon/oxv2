## Observed

After a server restart + runner re-registration cycle, \`ox-ctl exec show\` reported an execution's step as \`running\` on \`run-0000\` for 114 minutes, while \`ox-ctl status\` reported \`run-0000\` as \`idle\`. The runner had been reassigned to a different execution without any terminal event closing the old attempt.

Timeline from event log (exec \`e-1776443865-370\`):

- seq 396-397: step.dispatched + step.running for exec-370/implement on run-0000
- [server down overnight]
- seq 400: server.ready (first restart)
- seq 402: runner.heartbeat_missed run-0000 exec-370/implement (check_loop re-fires)
- seq 405: runner.registered run-0000 (runner comes online)
- seq 406-407: herder re-readies + re-dispatches; step.running for exec-370/implement
- seq 409: server.ready (second restart, ~1 min later)
- seq 413: runner.registered run-0000 (runner reconnects after second restart) — herder's \`RunnerView\` is clobbered to idle
- seq 414: herder dispatches **exec-366/implement** to run-0000 (treats it as idle)
- seq 419: step.confirmed for exec-366/implement

exec-370/implement never gets a terminal event. Its projection attempt stays \`status=Running, runner_id=run-0000\` forever; execution stays \`running\`.

## Root cause

The server owns the event log, projections, and heartbeat table — i.e. all three signals needed to detect drift. But \`check_loop\` (pool.rs) only scans **runner→heartbeat** mismatches, not **attempt→runner** mismatches. Once a runner is reassigned and starts heartbeating for the new execution, the abandoned attempt is invisible.

Four code paths can orphan a running attempt:
- runner re-register (\`pool.rs::register\`) → projection \`RUNNER_REGISTERED\` clears \`current_step\` (projections.rs:155)
- step dispatch to already-busy runner → \`STEP_DISPATCHED\` overwrites \`current_step\` (projections.rs:189)
- manual drain (\`pool.rs::drain\`) → only marks runner Drained, doesn't close attempt
- startup orphan sweep (\`pool.rs::sweep_orphans\`) → same gap as manual drain

Plus a latent contributor: \`RunnerId::generate()\` is a process-local \`AtomicU16\`, so \"run-0000\" from yesterday collides with a fresh \"run-0000\" today after server restart.

## Invariant

> A step attempt's runner binding (\`attempt.runner_id\` + \`attempt.status = Running\`) is a lease that can only be released by a terminal step event (\`step.confirmed\`, \`step.failed\`, \`step.timeout\`).

## Fix: attempt-side sweep (mirrors sweep_orphans for runners)

Add \`scan_orphan_attempts(bus)\` in ox-server/src/pool.rs. For each execution with \`attempt.status ∈ {Dispatched, Running}\`:

1. Look up \`attempt.runner_id\` in pool projection.
2. Three cases:
   - **Runner gone** (drained or not in projection) → emit \`step.failed\` with reason \"orphaned: runner drained/gone\".
   - **Runner present, \`current_step\` points to a different (exec, step, attempt)** → emit \`step.failed\` with reason \"orphaned: runner reassigned\".
   - **Runner present, \`current_step\` matches** → healthy (runner.heartbeat_missed already covers heartbeat staleness).

Run the scan:
- in \`check_loop\` (live 15s tick) — catches drain, re-dispatch, and any future offending path
- at startup alongside \`sweep_orphans\` — catches the restart-restore case in this bug

## Slices

- Slice 1: \`scan_orphan_attempts\` pure helper + unit tests for all three cases
- Slice 2: wire into check_loop; emit step.failed; integration test for dispatch collision
- Slice 3: call at startup alongside sweep_orphans; test replays corrupted DB shape

## Out of scope (file separately if pursued)

- Guard in \`dispatch_step\` that rejects dispatching to an already-busy runner (preventive; sweep is reactive)
- Globally-unique \`RunnerId\` (encode server epoch or UUID to stop \"run-0000\" collision across restarts)
- Runner-side resume-after-reconnect
