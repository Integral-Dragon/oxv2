Red: fire cx.task_ready twice for same consultation node, assert exactly one execution.created.

Green minimum:
- ExecutionOrigin + ChildKind enums in events.rs (PartialEq/Eq/Hash/Clone/Serde)
- Option<ExecutionOrigin> on ExecutionCreatedData (#[serde(default)])
- origin: ExecutionOrigin on ExecutionState (projections.rs:44)
- fallback_origin(vars) helper: task_id → CxNode, else Manual
- Replace 5 hardcoded task_id sites with origin equality
- Unify active() helper: API=running, herder=running|escalated
- CreateExecutionRequest carries origin; api.rs defaults to Manual