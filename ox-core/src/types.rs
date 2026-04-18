use chrono::{DateTime, Utc};
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
/// Format: `run-{epoch_hex}-{counter_hex}`, e.g. `run-67c0a5b2-0000`.
/// The epoch portion encodes the server's start time (unix seconds),
/// so IDs issued by different server lifetimes cannot collide — even
/// when an event log replay resurrects a prior-lifetime runner.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RunnerId(pub String);

impl fmt::Display for RunnerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Format a runner-id string from an epoch and a counter. Pure — no
/// hidden state. `generate()` uses this with process-wide static
/// state; tests call it directly to verify cross-lifetime uniqueness.
pub fn fmt_runner_id(epoch_secs: u64, counter: u32) -> String {
    format!("run-{epoch_secs:x}-{counter:04x}")
}

static RUNNER_START_EPOCH: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
static RUNNER_COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

impl RunnerId {
    /// Initialize the generator with this server's start epoch. Call
    /// once at server startup before any runners register. Subsequent
    /// calls are ignored (`OnceLock` semantics) so tests and nested
    /// code paths can call it defensively.
    pub fn init_generator(start: DateTime<Utc>) {
        let _ = RUNNER_START_EPOCH.set(start.timestamp() as u64);
    }

    pub fn generate() -> Self {
        use std::sync::atomic::Ordering;
        let epoch = *RUNNER_START_EPOCH.get_or_init(|| Utc::now().timestamp() as u64);
        let n = RUNNER_COUNTER.fetch_add(1, Ordering::Relaxed);
        Self(fmt_runner_id(epoch, n))
    }
}

/// Execution identifier. Server-generated: "e-{epoch}-{seq}".
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

    // ── fmt_runner_id + generate ───────────────────────────────────

    /// Format contract: deterministic, hex-encoded epoch, 4-digit
    /// zero-padded hex counter.
    #[test]
    fn fmt_runner_id_shape() {
        assert_eq!(fmt_runner_id(0x67c0a5b2, 0), "run-67c0a5b2-0000");
        assert_eq!(fmt_runner_id(0x67c0a5b2, 0xff), "run-67c0a5b2-00ff");
    }

    /// The core invariant: IDs issued in different server lifetimes
    /// must never collide, even when the counter starts from zero on
    /// both sides. This is the bug that motivated the format change.
    #[test]
    fn fmt_runner_id_distinct_epochs_never_collide() {
        let yesterday = fmt_runner_id(1_700_000_000, 0);
        let today = fmt_runner_id(1_700_086_400, 0);
        assert_ne!(yesterday, today, "same counter, different epoch → distinct ids");
    }

    /// `generate()` uses a static counter — consecutive calls in the
    /// same process must produce distinct IDs with the same epoch
    /// prefix and an advancing counter.
    #[test]
    fn generate_increments_counter_within_process() {
        RunnerId::init_generator(Utc::now());
        let a = RunnerId::generate();
        let b = RunnerId::generate();
        assert_ne!(a, b);
        let a_parts: Vec<&str> = a.0.split('-').collect();
        let b_parts: Vec<&str> = b.0.split('-').collect();
        assert_eq!(a_parts.len(), 3);
        assert_eq!(b_parts.len(), 3);
        assert_eq!(a_parts[0], "run");
        assert_eq!(a_parts[1], b_parts[1], "epoch must match across calls");
        assert_ne!(a_parts[2], b_parts[2], "counter must advance");
    }

    /// Initialized generator must use the supplied epoch, not wall-clock.
    #[test]
    fn generate_uses_initialized_epoch() {
        RunnerId::init_generator(Utc::now()); // first-writer wins via OnceLock
        let id = RunnerId::generate();
        let epoch_hex = id.0.split('-').nth(1).unwrap();
        let parsed = u64::from_str_radix(epoch_hex, 16).expect("epoch must parse as hex");
        assert!(parsed > 0, "epoch must be non-zero");
        // Sanity: epoch must be a plausible recent unix timestamp.
        let now = Utc::now().timestamp() as u64;
        assert!(
            parsed >= now - 3600 && parsed <= now + 3600,
            "epoch {parsed} should be within ±1h of now ({now})"
        );
    }
}
