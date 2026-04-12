use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::types::{ExecutionId, RunnerId, Seq};

/// Common envelope for all events in the log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventEnvelope {
    pub seq: Seq,
    pub ts: DateTime<Utc>,
    pub event_type: EventType,
    pub data: serde_json::Value,
}

/// All event types in the system.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EventType {
    // Server
    #[serde(rename = "server.ready")]
    ServerReady,
    // Runner
    #[serde(rename = "runner.registered")]
    RunnerRegistered,
    #[serde(rename = "runner.drained")]
    RunnerDrained,
    #[serde(rename = "runner.heartbeat_missed")]
    RunnerHeartbeatMissed,
    // Triggers
    #[serde(rename = "trigger.failed")]
    TriggerFailed,
    // Execution
    #[serde(rename = "execution.created")]
    ExecutionCreated,
    #[serde(rename = "execution.completed")]
    ExecutionCompleted,
    #[serde(rename = "execution.escalated")]
    ExecutionEscalated,
    #[serde(rename = "execution.cancelled")]
    ExecutionCancelled,
    // Step
    #[serde(rename = "step.dispatched")]
    StepDispatched,
    #[serde(rename = "step.running")]
    StepRunning,
    #[serde(rename = "step.done")]
    StepDone,
    #[serde(rename = "step.signals")]
    StepSignals,
    #[serde(rename = "step.confirmed")]
    StepConfirmed,
    #[serde(rename = "step.failed")]
    StepFailed,
    #[serde(rename = "step.advanced")]
    StepAdvanced,
    #[serde(rename = "step.timeout")]
    StepTimeout,
    #[serde(rename = "step.retrying")]
    StepRetrying,
    // Artifact
    #[serde(rename = "artifact.declared")]
    ArtifactDeclared,
    #[serde(rename = "artifact.closed")]
    ArtifactClosed,
    // Secrets
    #[serde(rename = "secret.set")]
    SecretSet,
    #[serde(rename = "secret.deleted")]
    SecretDeleted,
    // cx
    #[serde(rename = "cx.task_ready")]
    CxTaskReady,
    #[serde(rename = "cx.task_claimed")]
    CxTaskClaimed,
    #[serde(rename = "cx.task_integrated")]
    CxTaskIntegrated,
    #[serde(rename = "cx.task_shadowed")]
    CxTaskShadowed,
    #[serde(rename = "cx.comment_added")]
    CxCommentAdded,
    #[serde(rename = "cx.phase_complete")]
    CxPhaseComplete,
    // Git
    #[serde(rename = "git.branch_pushed")]
    GitBranchPushed,
    #[serde(rename = "git.merged")]
    GitMerged,
    #[serde(rename = "git.merge_failed")]
    GitMergeFailed,
}

// ── Event data structs ──────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunnerRegisteredData {
    pub runner_id: RunnerId,
    pub environment: String,
    pub labels: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunnerDrainedData {
    pub runner_id: RunnerId,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunnerHeartbeatMissedData {
    pub runner_id: RunnerId,
    pub last_seen: DateTime<Utc>,
    pub grace_period_secs: u64,
    /// The step the runner was working on when it went stale (if any).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionCreatedData {
    pub execution_id: ExecutionId,
    pub workflow: String,
    pub trigger: String,
    #[serde(default)]
    pub vars: HashMap<String, String>,
    /// Origin of this execution. Optional on the wire for backward compat
    /// with pre-refactor event logs; resolved to a concrete value at
    /// projection time via `fallback_origin` when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<ExecutionOrigin>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionCompletedData {
    pub execution_id: ExecutionId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionEscalatedData {
    pub execution_id: ExecutionId,
    pub step: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionCancelledData {
    pub execution_id: ExecutionId,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepDispatchedData {
    pub execution_id: ExecutionId,
    pub step: String,
    pub attempt: u32,
    pub runner_id: RunnerId,
    /// Secret names referenced by this step (values NOT included).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secret_refs: Vec<String>,
    pub runtime: serde_json::Value,
    pub workspace: serde_json::Value,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<ArtifactDecl>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactDecl {
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepRunningData {
    pub execution_id: ExecutionId,
    pub step: String,
    pub attempt: u32,
    /// TCP address for interactive (tty) sessions, e.g. "192.168.1.5:43210".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connect_addr: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepDoneData {
    pub execution_id: ExecutionId,
    pub step: String,
    pub attempt: u32,
    pub output: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepSignalsData {
    pub execution_id: ExecutionId,
    pub step: String,
    pub attempt: u32,
    pub signals: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepConfirmedData {
    pub execution_id: ExecutionId,
    pub step: String,
    pub attempt: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metrics: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepFailedData {
    pub execution_id: ExecutionId,
    pub step: String,
    pub attempt: u32,
    pub error: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepTimeoutData {
    pub execution_id: ExecutionId,
    pub step: String,
    pub attempt: u32,
    pub timeout_secs: u64,
    pub runner_id: RunnerId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepAdvancedData {
    pub execution_id: ExecutionId,
    pub from_step: String,
    pub to_step: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepRetryingData {
    pub execution_id: ExecutionId,
    pub step: String,
    pub attempt: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactDeclaredData {
    pub execution_id: ExecutionId,
    pub step: String,
    pub artifact: String,
    pub source: String,
    pub streaming: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactClosedData {
    pub execution_id: ExecutionId,
    pub step: String,
    pub artifact: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretSetData {
    pub name: String,
    /// Present in the event log, redacted from SSE broadcast.
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretDeletedData {
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CxTaskReadyData {
    pub node_id: String,
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
}

/// Trigger-firing event context. Exposes the subset of event payload fields
/// that can be referenced as `{event.X}` inside a `[trigger.vars]` block.
///
/// v1 supports cx events only; workflow-chaining variants are deferred.
#[derive(Debug, Clone)]
pub enum EventContext {
    CxTaskReady { node_id: String },
    CxTaskClaimed { node_id: String },
    CxTaskIntegrated { node_id: String },
    CxTaskShadowed { node_id: String, reason: String },
    CxCommentAdded {
        node_id: String,
        tag: Option<String>,
        author: Option<String>,
    },
}

impl EventContext {
    /// Resolve a dotted path like `event.node_id` to its string value.
    /// Returns `None` if the path is not defined for this variant.
    pub fn resolve(&self, path: &str) -> Option<String> {
        let field = path.strip_prefix("event.")?;
        match (self, field) {
            (Self::CxTaskReady { node_id }, "node_id") => Some(node_id.clone()),
            (Self::CxTaskClaimed { node_id }, "node_id") => Some(node_id.clone()),
            (Self::CxTaskIntegrated { node_id }, "node_id") => Some(node_id.clone()),
            (Self::CxTaskShadowed { node_id, .. }, "node_id") => Some(node_id.clone()),
            (Self::CxTaskShadowed { reason, .. }, "reason") => Some(reason.clone()),
            (Self::CxCommentAdded { node_id, .. }, "node_id") => Some(node_id.clone()),
            (Self::CxCommentAdded { tag, .. }, "tag") => tag.clone(),
            (Self::CxCommentAdded { author, .. }, "author") => author.clone(),
            _ => None,
        }
    }
}

/// Identity of the event that caused an execution to be created.
///
/// Persisted on `execution.created`. The dedup key for trigger evaluation is
/// `(workflow, origin)` — structural equality per variant. Every execution
/// has exactly one origin; there is no propagation up an ancestor chain.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ExecutionOrigin {
    /// Fired from a cx event tied to a specific node. The canonical origin
    /// for `cx.*` triggers.
    CxNode { node_id: String },
    /// Chained from a prior execution via a workflow or step event trigger.
    /// v1 does not wire the workflow-chaining path; the variant exists so
    /// the data model is stable.
    Execution {
        parent_execution_id: ExecutionId,
        parent_step: Option<String>,
        kind: ChildKind,
    },
    /// Direct API call or CLI trigger with no event context.
    Manual {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        user: Option<String>,
    },
}

/// What child-triggering workflow event produced an `Execution` origin.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChildKind {
    Escalated,
    Completed,
    StepCompleted,
    StepFailed,
}

/// Best-effort synthesis of an origin for pre-refactor events that lack
/// one on the wire. Old `execution.created` payloads that carried
/// `vars["task_id"]` are treated as `CxNode`; everything else as `Manual`.
///
/// Live code paths should always pass an explicit origin to
/// `ExecutionCreatedData`. This helper exists solely for event-log replay
/// compatibility.
pub fn fallback_origin(vars: &HashMap<String, String>) -> ExecutionOrigin {
    match vars.get("task_id") {
        Some(node_id) => ExecutionOrigin::CxNode {
            node_id: node_id.clone(),
        },
        None => ExecutionOrigin::Manual { user: None },
    }
}

/// Structural dedup predicate. Returns `true` if any element of `existing`
/// has the same `(origin, workflow)` pair and its status is considered
/// active for blocking purposes.
///
/// The `is_active` callback is supplied by the caller because the herder
/// and the API handler use different status liveness rules (the API blocks
/// on `running` only; the herder blocks on `running|escalated`).
pub fn is_origin_active<'a, I>(
    existing: I,
    origin: &ExecutionOrigin,
    workflow: &str,
    is_active: impl Fn(&str) -> bool,
) -> bool
where
    I: IntoIterator<Item = (&'a ExecutionOrigin, &'a str, &'a str)>,
{
    existing
        .into_iter()
        .any(|(o, w, s)| o == origin && w == workflow && is_active(s))
}

// ── Trigger failure events ─────────────────────────────────────────

/// Recorded when a trigger matches a firing event but cannot produce
/// a valid execution. Always surfaces deterministically — a bad
/// `[trigger.vars]` template will emit on every SSE replay, so the
/// herder guards emission behind `!replaying`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TriggerFailedData {
    /// The `seq` of the event that caused this trigger to fire.
    /// Lets a UI correlate "which cx event caused this failure".
    pub source_seq: Seq,
    /// The matched trigger's `on` field (e.g. `"cx.task_ready"`).
    pub on: String,
    /// The matched trigger's `tag`, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    /// The workflow the trigger would have fired.
    pub workflow: String,
    pub reason: TriggerFailureReason,
}

/// Why a trigger failed to create an execution. Discriminated so UIs
/// and operators can act on the category without parsing free-text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TriggerFailureReason {
    /// A `{event.X}` template referenced a field the firing event
    /// does not expose.
    MissingEventField { path: String },
    /// The interpolated vars map failed `WorkflowDef::validate_vars`.
    ValidationFailed { message: String },
    /// The trigger's `workflow` is not loaded in the current config.
    UnknownWorkflow,
}

impl TriggerFailedData {
    /// Build a failure record from a missing-field interpolation error.
    pub fn from_missing_field(
        source_seq: Seq,
        on: &str,
        tag: Option<&str>,
        workflow: &str,
        missing_path: String,
    ) -> Self {
        todo!("slice C: construct TriggerFailedData::MissingEventField")
    }

    /// Build a failure record from a `WorkflowDef::validate_vars` error.
    pub fn from_validation_error(
        source_seq: Seq,
        on: &str,
        tag: Option<&str>,
        workflow: &str,
        message: String,
    ) -> Self {
        todo!("slice C: construct TriggerFailedData::ValidationFailed")
    }

    /// Build a failure record when the trigger's workflow doesn't exist.
    pub fn for_unknown_workflow(
        source_seq: Seq,
        on: &str,
        tag: Option<&str>,
        workflow: &str,
    ) -> Self {
        todo!("slice C: construct TriggerFailedData::UnknownWorkflow")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CxTaskClaimedData {
    pub node_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub part: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CxTaskIntegratedData {
    pub node_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CxTaskShadowedData {
    pub node_id: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CxCommentAddedData {
    pub node_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CxPhaseCompleteData {
    pub node_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitBranchPushedData {
    pub branch: String,
    pub sha: String,
    pub execution_id: ExecutionId,
    pub step: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitMergedData {
    pub branch: String,
    pub into: String,
    pub sha: String,
    pub execution_id: ExecutionId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitMergeFailedData {
    pub branch: String,
    pub into: String,
    pub reason: String,
    pub execution_id: ExecutionId,
}

// ── Redaction ��──────────────────────────────────────────────────────

impl EventEnvelope {
    /// Return a copy suitable for SSE broadcast — secret values stripped.
    pub fn redacted_for_sse(&self) -> Self {
        if self.event_type == EventType::SecretSet {
            let mut data = self.data.clone();
            if let Some(obj) = data.as_object_mut() {
                obj.remove("value");
            }
            Self {
                seq: self.seq,
                ts: self.ts,
                event_type: self.event_type.clone(),
                data,
            }
        } else {
            self.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_type_serde() {
        let t = EventType::StepConfirmed;
        let json = serde_json::to_string(&t).unwrap();
        assert_eq!(json, "\"step.confirmed\"");
        let back: EventType = serde_json::from_str(&json).unwrap();
        assert_eq!(back, EventType::StepConfirmed);
    }

    #[test]
    fn secret_set_redaction() {
        let envelope = EventEnvelope {
            seq: Seq(1),
            ts: Utc::now(),
            event_type: EventType::SecretSet,
            data: serde_json::to_value(SecretSetData {
                name: "api_key".into(),
                value: "sk-secret-123".into(),
            })
            .unwrap(),
        };

        let redacted = envelope.redacted_for_sse();
        let obj = redacted.data.as_object().unwrap();
        assert!(obj.contains_key("name"));
        assert!(!obj.contains_key("value"));
    }

    #[test]
    fn non_secret_event_not_redacted() {
        let envelope = EventEnvelope {
            seq: Seq(2),
            ts: Utc::now(),
            event_type: EventType::RunnerRegistered,
            data: serde_json::to_value(RunnerRegisteredData {
                runner_id: RunnerId("run-0001".into()),
                environment: "test".into(),
                labels: HashMap::new(),
            })
            .unwrap(),
        };

        let redacted = envelope.redacted_for_sse();
        assert_eq!(
            serde_json::to_string(&envelope.data).unwrap(),
            serde_json::to_string(&redacted.data).unwrap()
        );
    }

    // ── slice B: ExecutionOrigin ───────────────────────────────────────

    #[test]
    fn execution_origin_structural_equality() {
        let a = ExecutionOrigin::CxNode {
            node_id: "aJuO".into(),
        };
        let b = ExecutionOrigin::CxNode {
            node_id: "aJuO".into(),
        };
        let c = ExecutionOrigin::CxNode {
            node_id: "different".into(),
        };
        let m = ExecutionOrigin::Manual { user: None };

        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_ne!(a, m);
    }

    #[test]
    fn fallback_origin_synthesizes_cx_node_from_task_id() {
        let mut vars = HashMap::new();
        vars.insert("task_id".into(), "aJuO".into());
        let o = fallback_origin(&vars);
        assert_eq!(
            o,
            ExecutionOrigin::CxNode {
                node_id: "aJuO".into()
            }
        );
    }

    #[test]
    fn fallback_origin_without_task_id_is_manual() {
        let vars = HashMap::new();
        let o = fallback_origin(&vars);
        assert_eq!(o, ExecutionOrigin::Manual { user: None });
    }

    #[test]
    fn execution_created_data_round_trips_with_and_without_origin() {
        // New-shape payload: round-trips with origin set.
        let with_origin = ExecutionCreatedData {
            execution_id: ExecutionId("e-1".into()),
            workflow: "consultation".into(),
            trigger: "cx.task_ready".into(),
            vars: HashMap::from([("branch".into(), "aJuO".into())]),
            origin: Some(ExecutionOrigin::CxNode {
                node_id: "aJuO".into(),
            }),
        };
        let json = serde_json::to_value(&with_origin).unwrap();
        let back: ExecutionCreatedData = serde_json::from_value(json).unwrap();
        assert_eq!(back.origin, with_origin.origin);
        assert_eq!(back.vars, with_origin.vars);

        // Legacy-shape payload: no origin field on the wire, deserializes as None.
        let legacy_json = serde_json::json!({
            "execution_id": "e-old",
            "workflow": "code-task",
            "trigger": "cx.task_ready",
            "vars": { "task_id": "aJuO" }
        });
        let legacy: ExecutionCreatedData = serde_json::from_value(legacy_json).unwrap();
        assert_eq!(legacy.origin, None);
        // The projection's fallback step would synthesize an origin from vars:
        assert_eq!(
            fallback_origin(&legacy.vars),
            ExecutionOrigin::CxNode {
                node_id: "aJuO".into()
            }
        );
    }

    // ── slice C: TriggerFailed ─────────────────────────────────────────

    #[test]
    fn trigger_failed_event_type_serializes_to_dotted_name() {
        let t = EventType::TriggerFailed;
        let json = serde_json::to_string(&t).unwrap();
        assert_eq!(json, "\"trigger.failed\"");
        let back: EventType = serde_json::from_str(&json).unwrap();
        assert_eq!(back, EventType::TriggerFailed);
    }

    #[test]
    fn trigger_failed_data_round_trips_missing_field_reason() {
        let data = TriggerFailedData::from_missing_field(
            Seq(42),
            "cx.task_ready",
            Some("workflow:consultation"),
            "consultation",
            "event.bogus".into(),
        );
        assert_eq!(data.source_seq, Seq(42));
        assert_eq!(data.on, "cx.task_ready");
        assert_eq!(data.tag.as_deref(), Some("workflow:consultation"));
        assert_eq!(data.workflow, "consultation");
        assert_eq!(
            data.reason,
            TriggerFailureReason::MissingEventField {
                path: "event.bogus".into()
            }
        );

        let json = serde_json::to_value(&data).unwrap();
        let back: TriggerFailedData = serde_json::from_value(json).unwrap();
        assert_eq!(data, back);
    }

    #[test]
    fn trigger_failed_data_round_trips_validation_reason() {
        let data = TriggerFailedData::from_validation_error(
            Seq(7),
            "cx.task_ready",
            None,
            "code-task",
            "missing required variable: task_id".into(),
        );
        assert_eq!(
            data.reason,
            TriggerFailureReason::ValidationFailed {
                message: "missing required variable: task_id".into()
            }
        );
        let json = serde_json::to_value(&data).unwrap();
        let back: TriggerFailedData = serde_json::from_value(json).unwrap();
        assert_eq!(data, back);
    }

    #[test]
    fn trigger_failed_data_round_trips_unknown_workflow_reason() {
        let data = TriggerFailedData::for_unknown_workflow(
            Seq(1),
            "cx.task_ready",
            Some("workflow:ghost"),
            "ghost",
        );
        assert_eq!(data.reason, TriggerFailureReason::UnknownWorkflow);
        let json = serde_json::to_value(&data).unwrap();
        let back: TriggerFailedData = serde_json::from_value(json).unwrap();
        assert_eq!(data, back);
    }

    #[test]
    fn trigger_failure_reason_is_tagged_on_wire() {
        // Discriminator lives in the serialized payload so a UI can
        // switch on it without deserializing into the full enum.
        let reason = TriggerFailureReason::MissingEventField {
            path: "event.bogus".into(),
        };
        let json = serde_json::to_value(&reason).unwrap();
        assert_eq!(json["type"], "missing_event_field");
        assert_eq!(json["path"], "event.bogus");
    }

    #[test]
    fn is_origin_active_matches_on_origin_workflow_and_liveness() {
        let origin_a = ExecutionOrigin::CxNode {
            node_id: "aJuO".into(),
        };
        let origin_b = ExecutionOrigin::CxNode {
            node_id: "other".into(),
        };
        let wf = "consultation";
        let active = |s: &str| s == "running";

        // Match on origin + workflow + active status
        let existing = [(&origin_a, wf, "running")];
        assert!(is_origin_active(
            existing.iter().map(|(o, w, s)| (*o, *w, *s)),
            &origin_a,
            wf,
            active
        ));

        // Different origin → no match
        let existing = [(&origin_b, wf, "running")];
        assert!(!is_origin_active(
            existing.iter().map(|(o, w, s)| (*o, *w, *s)),
            &origin_a,
            wf,
            active
        ));

        // Different workflow → no match
        let existing = [(&origin_a, "other-wf", "running")];
        assert!(!is_origin_active(
            existing.iter().map(|(o, w, s)| (*o, *w, *s)),
            &origin_a,
            wf,
            active
        ));

        // Completed status is not active under this rule
        let existing = [(&origin_a, wf, "completed")];
        assert!(!is_origin_active(
            existing.iter().map(|(o, w, s)| (*o, *w, *s)),
            &origin_a,
            wf,
            active
        ));
    }
}
