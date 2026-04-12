Red: integration test that fires cx.task_ready for a consultation-tagged node and asserts execution vars contain branch=<node_id>, not task_id. Repeat for code-task with task_id populated.

Green minimum:
- Add TriggerDef.vars field (workflow.rs:202)
- Minimal EventContext enum (CxTaskReady { node_id }) in ox-core/src/events.rs
- TriggerDef::build_vars(&self, ctx) with dumb {event.X} interpolator
- Herder evaluate_triggers_for_node: replace hardcoded task_id insert with build_vars (herder.rs:~920–950)
- api.rs evaluate_triggers: same replacement (api.rs:~1065)
- Update defaults/workflows/triggers.toml with [trigger.vars] blocks

Out of scope: ExecutionOrigin, trigger.failed, --task removal.