ox-core/src/workflow.rs:

- extend RetryTracker::record_failure(step, max_retries, force_escalate: bool) -> RetryDecision
- when force_escalate: return Exhausted immediately (and reset attempts so a clean rerun isn't poisoned)

ox-herder/src/herder.rs around line 620:
- before record_failure: evaluate just-emitted signals against the step's runtime failure_signals; if any matched signal has retriable=false, pass force_escalate=true

Red tests:
- record_failure(.., force_escalate: true) returns Exhausted even when attempts < max_retries
- (optional) herder-level: a step whose signals include a non-retriable name escalates on attempt 1