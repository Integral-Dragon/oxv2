## Observed

In a healthy 2-runner pool, a transient heartbeat stall on \`run-0001\` caused the herder to permanently lose track of it. Subsequent executions that reached \`Ready\` phase found no idle runners and stalled indefinitely, even though \`run-0001\` recovered within seconds, finished its step, and was idle from the server's perspective.

Timeline (from \`.ox/run/logs/herder.log\` + event log):

- \`20:13:05\` herder dispatches \`e-7/propose\` to \`run-0001\`
- \`20:14:11\` server emits \`runner.heartbeat_missed\` (transient stall)
- \`20:14:58\` \`run-0001\` completes — \`step.done\` / \`step.confirmed\` arrive
- \`20:14:58\` herder advances \`e-7\` to \`review-plan\` but **no dispatch line** follows
- \`e-7\` stuck forever; DB shows \`run-0001\` heartbeating and idle throughout

Meanwhile \`e-8\` on \`run-0000\` progressed normally.

## Root cause

\`ox-herder/src/herder.rs\` \`RUNNER_HEARTBEAT_MISSED\` handler unconditionally does \`self.runners.remove(...)\`. But a heartbeat miss is a **\"this step might be orphaned\"** signal from the server, not a death signal — the server dedups the event and never auto-drains. The only death signal is \`runner.drained\` (emitted on explicit drain).

Once the herder removes the runner, there is no path to re-learn about it: \`runner.registered\` only fires on fresh registration. \`find_idle_runner()\` walks the herder's local map, so the runner is effectively lost.

## Fix

Drop the \`self.runners.remove(&d.runner_id.0)\` line in the \`RUNNER_HEARTBEAT_MISSED\` handler. Keep the orphan re-dispatch logic. Only \`RUNNER_DRAINED\` should evict from the local map.

This is correct because:
- If the runner is alive (blip case): it finishes its step, \`step.confirmed\` frees the runner via \`free_runner_for_step\`, and it becomes available for future dispatch.
- If the runner is truly dead: the orphaned step still re-readies via the existing handler. A separate \`step.timeout\` event (or an explicit drain) is the correct mechanism for truly-dead cleanup, and already exists.

## Repro

1. 2-runner pool, 2 ready executions, both dispatched.
2. Cause one runner to briefly exceed heartbeat grace (e.g. \`SIGSTOP\` for 60s, then \`SIGCONT\`).
3. Observe: the stalled runner's step still completes, but subsequent steps for that execution never dispatch.