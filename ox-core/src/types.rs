use serde::{Deserialize, Serialize};
use std::fmt;

/// Monotonically increasing event sequence number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Seq(pub u64);

impl fmt::Display for Seq {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Runner identifier. Assigned by ox-server on registration.
/// Format: "run-{4hex}" e.g. "run-4a2f"
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RunnerId(pub String);

impl fmt::Display for RunnerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl RunnerId {
    pub fn generate() -> Self {
        use std::sync::atomic::{AtomicU16, Ordering};
        static COUNTER: AtomicU16 = AtomicU16::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        Self(format!("run-{n:04x}"))
    }
}

/// Execution identifier. Format: "{task_id}-e{N}"
/// e.g. "aJuO-e1". N is sequential per task.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ExecutionId(pub String);

impl fmt::Display for ExecutionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Addresses a specific step attempt within an execution.
/// Format: "{execution_id}/{step_name}/{attempt}"
/// e.g. "aJuO-e1/propose/2"
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct StepAttemptId {
    pub execution_id: ExecutionId,
    pub step: String,
    pub attempt: u32,
}

impl fmt::Display for StepAttemptId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}/{}", self.execution_id, self.step, self.attempt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runner_id_format() {
        let id = RunnerId("run-4a2f".into());
        assert_eq!(id.to_string(), "run-4a2f");
    }

    #[test]
    fn execution_id_format() {
        let id = ExecutionId("aJuO-e1".into());
        assert_eq!(id.to_string(), "aJuO-e1");
    }

    #[test]
    fn step_attempt_id_format() {
        let id = StepAttemptId {
            execution_id: ExecutionId("aJuO-e1".into()),
            step: "propose".into(),
            attempt: 2,
        };
        assert_eq!(id.to_string(), "aJuO-e1/propose/2");
    }

    #[test]
    fn seq_ordering() {
        assert!(Seq(1) < Seq(2));
        assert_eq!(Seq(42), Seq(42));
    }

    #[test]
    fn serde_round_trip() {
        let id = ExecutionId("test-e1".into());
        let json = serde_json::to_string(&id).unwrap();
        let back: ExecutionId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, back);
    }
}
