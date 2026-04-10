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
    pub task_id: String,
    pub workflow: String,
    pub trigger: String,
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
}
