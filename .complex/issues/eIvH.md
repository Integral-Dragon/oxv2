## Problem

\`ox-server/src/api.rs:622\` (\`dispatch_step\`) unconditionally appends \`STEP_DISPATCHED\`. If the target runner's projection shows \`current_step = (exec_A, step, attempt)\` and the dispatch is for a *different* \`(exec_B, step, attempt)\`, the projection at \`projections.rs:189\` silently overwrites \`current_step\`, orphaning exec_A's attempt. The orphan-attempt sweep (ys1X) cleans this up within 15s, but it's cleaner to close at dispatch time.

## Fix

Auto-close: before appending STEP_DISPATCHED, read pool projection. If target runner has a *different* \`(exec, step, attempt)\` in \`current_step\`, emit \`step.failed\` with reason \"orphaned: runner reassigned at dispatch time\" for the prior attempt, then emit the new STEP_DISPATCHED.

Same-attempt re-dispatch (the orphan-recovery path at \`projections.rs:202\`) must still work — skip the guard when the target \`(exec, step, attempt)\` matches.

## Out of scope

Rejecting with 409 Conflict was considered but requires herder changes (no existing error-handling path), so auto-close is strictly simpler.
