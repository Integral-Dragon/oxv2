use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::types::{ExecutionId, RunnerId, Seq};

// ── Canonical envelope ──────────────────────────────────────────────

/// Canonical event envelope. Every event in the log — whether emitted
/// by Ox internally or ingested from a watcher — has this shape.
///
/// - `source`: identifier of the emitter (`"ox"`, `"cx"`, `"github"`, ...).
/// - `kind`: the event kind string (e.g. `"execution.completed"`,
///   `"node.ready"`). Plain string on the wire, grep-able, extensible
///   without recompiling downstream consumers.
/// - `subject_id`: the source-native correlation key — what the event
///   is about. Empty string for events with no meaningful subject
///   (e.g. `server.ready`).
/// - `data`: free-form kind-specific payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventEnvelope {
    pub seq: Seq,
    pub ts: DateTime<Utc>,
    pub source: String,
    pub kind: String,
    pub subject_id: String,
    pub data: serde_json::Value,
}

/// Source identifier stamped on every event emitted internally by Ox.
pub const SOURCE_OX: &str = "ox";

/// Event kinds. Plain string constants so every emit site is grep-able
/// and a new kind costs one `pub const` — not an enum variant plus
/// serde rename plus projection arm.
pub mod kinds {
    pub const SERVER_READY: &str = "server.ready";

    pub const RUNNER_REGISTERED: &str = "runner.registered";
    pub const RUNNER_DRAINED: &str = "runner.drained";
    pub const RUNNER_HEARTBEAT_MISSED: &str = "runner.heartbeat_missed";
    pub const RUNNER_RECOVERED: &str = "runner.recovered";

    pub const TRIGGER_FAILED: &str = "trigger.failed";

    pub const EXECUTION_CREATED: &str = "execution.created";
    pub const EXECUTION_COMPLETED: &str = "execution.completed";
    pub const EXECUTION_ESCALATED: &str = "execution.escalated";
    pub const EXECUTION_CANCELLED: &str = "execution.cancelled";

    pub const STEP_DISPATCHED: &str = "step.dispatched";
    pub const STEP_RUNNING: &str = "step.running";
    pub const STEP_DONE: &str = "step.done";
    pub const STEP_SIGNALS: &str = "step.signals";
    pub const STEP_CONFIRMED: &str = "step.confirmed";
    pub const STEP_FAILED: &str = "step.failed";
    pub const STEP_ADVANCED: &str = "step.advanced";
    pub const STEP_TIMEOUT: &str = "step.timeout";
    pub const STEP_RETRYING: &str = "step.retrying";

    pub const ARTIFACT_DECLARED: &str = "artifact.declared";
    pub const ARTIFACT_CLOSED: &str = "artifact.closed";

    pub const SECRET_SET: &str = "secret.set";
    pub const SECRET_DELETED: &str = "secret.deleted";

    pub const GIT_BRANCH_PUSHED: &str = "git.branch_pushed";
    pub const GIT_MERGED: &str = "git.merged";
    pub const GIT_MERGE_FAILED: &str = "git.merge_failed";
}

// ── Ingest path ─────────────────────────────────────────────────────

/// One event in a watcher ingest batch. The server combines
/// `batch.source` with each `IngestEventData` to produce the canonical
/// `EventEnvelope`. `idempotency_key` is storage/ingest mechanics and
/// is not persisted on the envelope — it lands in the
/// `ingest_idempotency` table alone.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestEventData {
    pub kind: String,
    pub subject_id: String,
    pub idempotency_key: String,
    #[serde(default)]
    pub data: serde_json::Value,
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

/// Emitted when a runner that was previously marked stale
/// (`runner.heartbeat_missed`) starts heartbeating within the grace
/// period again. Paired with `heartbeat_missed` so projections can
/// track the full healthy↔stale transition without out-of-band
/// queries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunnerRecoveredData {
    pub runner_id: RunnerId,
    pub last_seen: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionCreatedData {
    pub execution_id: ExecutionId,
    pub workflow: String,
    pub trigger: String,
    #[serde(default)]
    pub vars: HashMap<String, String>,
    pub origin: ExecutionOrigin,
    /// Override for the step the herder schedules first. `None`
    /// (the default) means start at `workflow.first_step()` — the
    /// existing behavior. Set by `ox-ctl exec retry` to resume an
    /// escalated execution from the failed step instead of redoing
    /// confirmed work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_step: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionCompletedData {
    pub execution_id: ExecutionId,
    /// Workflow that just finished. Lets `[trigger.where]` on
    /// `execution.completed` filter by workflow (e.g. cx-surface only
    /// chains after `code-task`, not after itself).
    pub workflow: String,
    /// All input vars the execution was created with. Chained
    /// workflows can template from `{event.data.vars.<name>}`.
    #[serde(default)]
    pub vars: HashMap<String, String>,
    /// Origin of the completed execution — the cause that started it.
    /// Useful for correlating a completion back to the source fact
    /// (cx node id, linear issue, manual run, ...).
    pub origin: ExecutionOrigin,
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
    /// Per-match diagnostics for signals fired by declarative log-pattern
    /// detection. Parallel to `signals` (every entry's `name` also appears
    /// in `signals`), kept additive so older event records deserialize
    /// cleanly with an empty vec.
    #[serde(default)]
    pub signal_matches: Vec<SignalMatch>,
}

/// One log-pattern signal match: the configured signal name, the log
/// line that triggered it (so operators can see *why* a signal fired
/// without trawling logs), and the `retriable` bit copied from the
/// runtime config so the workflow engine can decide policy without
/// re-reading config.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SignalMatch {
    pub name: String,
    pub line: String,
    /// Whether this signal allows further retries. Defaults to `true`
    /// for events written before this field existed.
    #[serde(default = "default_signal_match_retriable")]
    pub retriable: bool,
}

fn default_signal_match_retriable() -> bool {
    true
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

// ── ExecutionOrigin ─────────────────────────────────────────────────

/// Identity of what caused an execution to be created.
///
/// `Event` carries the canonical envelope triplet `(source, kind,
/// subject_id)` plus the firing event's `seq` as a backreference. The
/// seq is informational — dedup matches on the triplet only via
/// [`origins_match_for_dedup`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ExecutionOrigin {
    /// Direct API call or CLI trigger with no event context.
    Manual {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        user: Option<String>,
    },
    /// Fired from an event on the bus. The tuple `(source, kind,
    /// subject_id)` identifies *what* the execution is about. `seq`
    /// points at the firing envelope for audit.
    Event {
        source: String,
        kind: String,
        subject_id: String,
        seq: Seq,
    },
}

impl Default for ExecutionOrigin {
    fn default() -> Self {
        Self::Manual { user: None }
    }
}

/// Structural match for trigger-dedup semantics. Two `Event` origins
/// are considered the same subject if their `(source, kind,
/// subject_id)` triplets match — the firing `seq` is ignored because a
/// retried watcher post would otherwise appear as a new subject.
/// `Manual` origins never dedup against anything.
pub fn origins_match_for_dedup(a: &ExecutionOrigin, b: &ExecutionOrigin) -> bool {
    match (a, b) {
        (
            ExecutionOrigin::Event {
                source: sa,
                kind: ka,
                subject_id: ia,
                ..
            },
            ExecutionOrigin::Event {
                source: sb,
                kind: kb,
                subject_id: ib,
                ..
            },
        ) => sa == sb && ka == kb && ia == ib,
        _ => false,
    }
}

/// Dedup predicate. Returns `true` if any element of `existing` has an
/// origin that matches `origin` for dedup purposes, its workflow is
/// `workflow`, and its status is considered active.
///
/// The `is_active` callback is supplied by the caller because the
/// herder and the API handler use different liveness rules (the API
/// blocks on `running` only; the herder blocks on `running|escalated`).
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
        .any(|(o, w, s)| origins_match_for_dedup(o, origin) && w == workflow && is_active(s))
}

// ── Envelope helpers ────────────────────────────────────────────────

impl EventEnvelope {
    /// Resolve `event.<path>` to a string value. `None` when the
    /// leaf is missing or not a scalar.
    ///
    /// Supported roots: `event.source`, `event.kind`,
    /// `event.subject_id`, `event.data.<key>...`.
    pub fn resolve(&self, path: &str) -> Option<String> {
        self.resolve_value(path).and_then(value_to_string)
    }

    /// Resolve `event.<path>` to a raw `serde_json::Value`. Used by
    /// `[trigger.where]` predicates that need to walk into arrays.
    pub fn resolve_value(&self, path: &str) -> Option<serde_json::Value> {
        let field = path.strip_prefix("event.")?;
        match field {
            "source" => Some(serde_json::Value::String(self.source.clone())),
            "kind" => Some(serde_json::Value::String(self.kind.clone())),
            "subject_id" => Some(serde_json::Value::String(self.subject_id.clone())),
            rest => rest
                .strip_prefix("data.")
                .and_then(|p| resolve_json_value(&self.data, p)),
        }
    }

    /// Return a copy suitable for SSE broadcast — secret values stripped.
    pub fn redacted_for_sse(&self) -> Self {
        if self.source == SOURCE_OX && self.kind == kinds::SECRET_SET {
            let mut data = self.data.clone();
            if let Some(obj) = data.as_object_mut() {
                obj.remove("value");
            }
            Self {
                seq: self.seq,
                ts: self.ts,
                source: self.source.clone(),
                kind: self.kind.clone(),
                subject_id: self.subject_id.clone(),
                data,
            }
        } else {
            self.clone()
        }
    }
}

/// Walk a dotted path into a JSON value. Returns `None` if any
/// segment is missing.
fn resolve_json_value(root: &serde_json::Value, path: &str) -> Option<serde_json::Value> {
    let mut cur = root;
    for seg in path.split('.') {
        cur = cur.get(seg)?;
    }
    Some(cur.clone())
}

fn value_to_string(value: serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) => Some(s),
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

// ── Trigger failure events ─────────────────────────────────────────

/// Recorded when a trigger matches a firing event but cannot produce
/// a valid execution. Always surfaces deterministically — a bad
/// `[trigger.vars]` template will emit on every SSE replay, so the
/// herder guards emission behind `!replaying`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TriggerFailedData {
    /// The `seq` of the event that caused this trigger to fire.
    /// Lets a UI correlate "which source event caused this failure".
    pub source_seq: Seq,
    /// The matched trigger's `on` field (source-native event kind,
    /// e.g. `"node.ready"`).
    pub on: String,
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
    fn new(source_seq: Seq, on: &str, workflow: &str, reason: TriggerFailureReason) -> Self {
        Self {
            source_seq,
            on: on.to_string(),
            workflow: workflow.to_string(),
            reason,
        }
    }

    pub fn from_missing_field(
        source_seq: Seq,
        on: &str,
        workflow: &str,
        missing_path: String,
    ) -> Self {
        Self::new(
            source_seq,
            on,
            workflow,
            TriggerFailureReason::MissingEventField { path: missing_path },
        )
    }

    pub fn from_validation_error(
        source_seq: Seq,
        on: &str,
        workflow: &str,
        message: String,
    ) -> Self {
        Self::new(
            source_seq,
            on,
            workflow,
            TriggerFailureReason::ValidationFailed { message },
        )
    }

    pub fn for_unknown_workflow(source_seq: Seq, on: &str, workflow: &str) -> Self {
        Self::new(
            source_seq,
            on,
            workflow,
            TriggerFailureReason::UnknownWorkflow,
        )
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope(source: &str, kind: &str, subject_id: &str, data: serde_json::Value) -> EventEnvelope {
        EventEnvelope {
            seq: Seq(1),
            ts: Utc::now(),
            source: source.into(),
            kind: kind.into(),
            subject_id: subject_id.into(),
            data,
        }
    }

    #[test]
    fn secret_set_redaction() {
        let ev = envelope(
            SOURCE_OX,
            kinds::SECRET_SET,
            "api_key",
            serde_json::to_value(SecretSetData {
                name: "api_key".into(),
                value: "sk-secret-123".into(),
            })
            .unwrap(),
        );
        let redacted = ev.redacted_for_sse();
        let obj = redacted.data.as_object().unwrap();
        assert!(obj.contains_key("name"));
        assert!(!obj.contains_key("value"));
    }

    #[test]
    fn non_secret_event_not_redacted() {
        let ev = envelope(
            SOURCE_OX,
            kinds::RUNNER_REGISTERED,
            "run-0001",
            serde_json::to_value(RunnerRegisteredData {
                runner_id: RunnerId("run-0001".into()),
                environment: "test".into(),
                labels: HashMap::new(),
            })
            .unwrap(),
        );
        let redacted = ev.redacted_for_sse();
        assert_eq!(
            serde_json::to_string(&ev.data).unwrap(),
            serde_json::to_string(&redacted.data).unwrap()
        );
    }

    #[test]
    fn resolve_top_level_and_data() {
        let ev = envelope(
            "cx",
            "node.ready",
            "Q6cY",
            serde_json::json!({ "title": "hi", "tags": ["workflow:code-task"] }),
        );
        assert_eq!(ev.resolve("event.source"), Some("cx".into()));
        assert_eq!(ev.resolve("event.kind"), Some("node.ready".into()));
        assert_eq!(ev.resolve("event.subject_id"), Some("Q6cY".into()));
        assert_eq!(ev.resolve("event.data.title"), Some("hi".into()));
        assert_eq!(ev.resolve("event.data.bogus"), None);
        assert_eq!(
            ev.resolve_value("event.data.tags"),
            Some(serde_json::json!(["workflow:code-task"]))
        );
    }

    // ── ExecutionOrigin ────────────────────────────────────────────────

    fn src_origin(subject: &str) -> ExecutionOrigin {
        ExecutionOrigin::Event {
            source: "cx".into(),
            kind: "node.ready".into(),
            subject_id: subject.into(),
            seq: Seq(42),
        }
    }

    #[test]
    fn origins_match_for_dedup_ignores_seq() {
        let a = src_origin("aJuO");
        let mut b = src_origin("aJuO");
        if let ExecutionOrigin::Event { seq, .. } = &mut b {
            *seq = Seq(999);
        }
        assert!(origins_match_for_dedup(&a, &b));
    }

    #[test]
    fn origins_match_for_dedup_respects_subject() {
        let a = src_origin("aJuO");
        let b = src_origin("other");
        assert!(!origins_match_for_dedup(&a, &b));
    }

    #[test]
    fn manual_never_matches_for_dedup() {
        let a = src_origin("aJuO");
        let m = ExecutionOrigin::Manual { user: None };
        assert!(!origins_match_for_dedup(&a, &m));
        assert!(!origins_match_for_dedup(&m, &m));
    }

    #[test]
    fn execution_completed_data_round_trips_workflow_vars_origin() {
        let data = ExecutionCompletedData {
            execution_id: ExecutionId("e-1".into()),
            workflow: "code-task".into(),
            vars: HashMap::from([("task_id".into(), "Q6cY".into())]),
            origin: src_origin("Q6cY"),
        };
        let json = serde_json::to_value(&data).unwrap();
        assert_eq!(json["workflow"], "code-task");
        assert_eq!(json["vars"]["task_id"], "Q6cY");
        let back: ExecutionCompletedData = serde_json::from_value(json).unwrap();
        assert_eq!(back.workflow, "code-task");
        assert_eq!(back.vars.get("task_id").map(String::as_str), Some("Q6cY"));
        assert_eq!(back.origin, data.origin);
    }

    #[test]
    fn execution_created_data_round_trips_origin() {
        let data = ExecutionCreatedData {
            execution_id: ExecutionId("e-1".into()),
            workflow: "code-task".into(),
            trigger: "node.ready".into(),
            vars: HashMap::from([("task_id".into(), "aJuO".into())]),
            origin: src_origin("aJuO"),
            start_step: None,
        };
        let json = serde_json::to_value(&data).unwrap();
        let back: ExecutionCreatedData = serde_json::from_value(json).unwrap();
        assert_eq!(back.origin, data.origin);
        assert_eq!(back.vars, data.vars);
    }

    // ── TriggerFailed ──────────────────────────────────────────────────

    #[test]
    fn trigger_failed_data_round_trips_missing_field_reason() {
        let data = TriggerFailedData::from_missing_field(
            Seq(42),
            "node.ready",
            "consultation",
            "event.bogus".into(),
        );
        let json = serde_json::to_value(&data).unwrap();
        let back: TriggerFailedData = serde_json::from_value(json).unwrap();
        assert_eq!(data, back);
    }

    #[test]
    fn trigger_failure_reason_is_tagged_on_wire() {
        let reason = TriggerFailureReason::MissingEventField {
            path: "event.bogus".into(),
        };
        let json = serde_json::to_value(&reason).unwrap();
        assert_eq!(json["type"], "missing_event_field");
        assert_eq!(json["path"], "event.bogus");
    }

    #[test]
    fn is_origin_active_matches_on_origin_workflow_and_liveness() {
        let origin_a = src_origin("aJuO");
        let origin_b = src_origin("other");
        let wf = "consultation";
        let active = |s: &str| s == "running";

        let existing = [(&origin_a, wf, "running")];
        assert!(is_origin_active(
            existing.iter().map(|(o, w, s)| (*o, *w, *s)),
            &origin_a,
            wf,
            active
        ));

        let existing = [(&origin_b, wf, "running")];
        assert!(!is_origin_active(
            existing.iter().map(|(o, w, s)| (*o, *w, *s)),
            &origin_a,
            wf,
            active
        ));

        let existing = [(&origin_a, "other-wf", "running")];
        assert!(!is_origin_active(
            existing.iter().map(|(o, w, s)| (*o, *w, *s)),
            &origin_a,
            wf,
            active
        ));

        let existing = [(&origin_a, wf, "completed")];
        assert!(!is_origin_active(
            existing.iter().map(|(o, w, s)| (*o, *w, *s)),
            &origin_a,
            wf,
            active
        ));
    }
}
