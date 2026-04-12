Red: trigger.vars referencing bogus event field → trigger.failed event with MissingEventField. Second test: validate_vars failure → ValidationFailed. No execution.created in either case.

Green minimum:
- TriggerFailedData, TriggerFailureReason, EventType::TriggerFailed
- TriggerError enum from build_vars
- Herder and api.rs trigger paths: on Err, append trigger.failed, continue
- Guard emission with if !self.replaying in herder
- client.rs: post_trigger_failed helper