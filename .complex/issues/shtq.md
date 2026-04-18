## Problem

\`RunnerId::generate()\` (\`ox-core/src/types.rs:28\`) uses a process-local \`AtomicU16\` that resets to 0 on every server restart. This is why the original ys1X bug manifested: a fresh \"run-0000\" today collided with a prior-lifetime \"run-0000\" still present in the replayed pool projection. The sweep fixes the symptom; this fix removes the cause.

## Fix

Encode the server start epoch into the ID: \`run-{epoch_secs_hex}-{counter_hex}\`. Two servers started at different times cannot collide, regardless of event-log replay.

Keeps the \`run-\` prefix so existing logs/tools stay readable. Example: \`run-67c0a5b2-0000\`.

## Why not init-counter-from-projection (Option A)

Considered but rejected — relies on the event log being complete. If a \`runner.registered\` event were ever truncated, the counter could wrap and collide. The epoch-encoded form is robust against any history gap.
