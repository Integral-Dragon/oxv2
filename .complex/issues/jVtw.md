## Motivation

sKFs (herder evicts runner on heartbeat_missed) was only caught in production. The root reason it wasn't caught in tests: the herder has no test coverage for the runner lifecycle state machine. Current tests (\`herder.rs\` \`mod tests\`) are almost entirely trigger-evaluation (\`source_event_fires_for_matching_source_and_kind\` etc.). The handler for \`RUNNER_HEARTBEAT_MISSED\` had zero assertions on it before sKFs.

## Scope

Add a test matrix covering the full runner lifecycle, exercising \`handle_sse_message\` end-to-end:

- \`registered\` → runner appears in local map as idle
- \`registered → step.dispatched\` → runner marked busy, current_execution/step set
- \`... → step.confirmed\` → runner freed, idle again, eligible for next dispatch
- \`... → heartbeat_missed (step in flight)\` → step re-readied, runner NOT evicted (the sKFs invariant)
- \`... → heartbeat_missed → runner.recovered\` → runner is idle and dispatchable (depends on 7rW7)
- \`... → drained\` → runner evicted from local map
- Full round: register → dispatch → stall → recover → confirm → dispatch-next

The existing sKFs regression test \`heartbeat_missed_does_not_evict_runner_from_local_map\` is the seed — extend it into the full matrix.

## Depends on

- 7rW7 for the \`runner.recovered\` transition tests.