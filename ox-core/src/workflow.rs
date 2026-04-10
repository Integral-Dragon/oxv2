use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

/// A complete workflow definition, loaded from TOML.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowDef {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default, rename = "step")]
    pub steps: Vec<StepDef>,
    #[serde(default, rename = "trigger")]
    pub triggers: Vec<TriggerDef>,
}

/// TOML file layout: [workflow] header + [[step]] + [[trigger]] arrays.
#[derive(Debug, Deserialize)]
struct WorkflowFile {
    workflow: WorkflowHeader,
    #[serde(default)]
    step: Vec<StepDef>,
    #[serde(default)]
    trigger: Vec<TriggerDef>,
}

#[derive(Debug, Deserialize)]
struct WorkflowHeader {
    name: String,
    #[serde(default)]
    description: String,
}

impl WorkflowDef {
    /// Parse a workflow definition from a TOML string.
    pub fn from_toml(toml_str: &str) -> Result<Self, toml::de::Error> {
        let file: WorkflowFile = toml::from_str(toml_str)?;
        Ok(Self {
            name: file.workflow.name,
            description: file.workflow.description,
            steps: file.step,
            triggers: file.trigger,
        })
    }

    /// Load a workflow definition from a TOML file.
    pub fn from_file(path: &Path) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        Ok(Self::from_toml(&content)?)
    }
}

/// A single step in a workflow.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepDef {
    pub name: String,
    #[serde(default)]
    pub runtime: Option<RuntimeSpec>,
    #[serde(default)]
    pub action: Option<String>,
    #[serde(default)]
    pub output: Option<String>,
    #[serde(default)]
    pub workspace: Option<WorkspaceDef>,
    #[serde(default)]
    pub artifacts: Vec<ArtifactDecl>,
    #[serde(default, rename = "transition")]
    pub transitions: Vec<TransitionDef>,
    #[serde(default)]
    pub max_retries: Option<u32>,
    #[serde(default)]
    pub max_visits: Option<u32>,
    #[serde(default)]
    pub max_visits_goto: Option<String>,
    #[serde(default)]
    pub on_fail: Option<String>,
    #[serde(default)]
    pub squash: bool,
}

/// Runtime specification on a step — selects which runtime to use and passes parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeSpec {
    #[serde(rename = "type")]
    pub runtime_type: String,
    #[serde(default)]
    pub tty: bool,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default, with = "option_duration_secs", skip_serializing_if = "Option::is_none")]
    pub timeout: Option<Duration>,
    /// All other fields are runtime-definition-specific.
    #[serde(flatten)]
    pub fields: HashMap<String, toml::Value>,
}

/// Workspace provisioning spec.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceDef {
    #[serde(default)]
    pub git_clone: bool,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub push: bool,
    #[serde(default)]
    pub read_only: bool,
}

/// A declared artifact on a step.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactDecl {
    pub name: String,
}

/// Transition routing based on step output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransitionDef {
    #[serde(rename = "match")]
    pub match_pattern: String,
    pub goto: String,
}

/// Trigger definition — creates executions in response to conditions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriggerDef {
    pub on: String,
    #[serde(default)]
    pub tag: Option<String>,
    #[serde(default)]
    pub state: Option<String>,
    pub workflow: String,
    #[serde(default)]
    pub poll_interval: Option<String>,
}

// ── Workflow Engine ─────────────────────────────────────────────────

/// Step graph indexed by name for O(1) lookup with preserved declaration order.
pub struct WorkflowEngine {
    pub name: String,
    pub steps: IndexMap<String, StepDef>,
    pub triggers: Vec<TriggerDef>,
}

/// Result of advancing to the next step.
#[derive(Debug, PartialEq, Eq)]
pub enum StepAdvance {
    Goto(String),
    Escalate,
    Complete,
}

impl WorkflowEngine {
    pub fn from_def(def: WorkflowDef) -> Self {
        let mut steps = IndexMap::new();
        for step in def.steps {
            steps.insert(step.name.clone(), step);
        }
        Self {
            name: def.name,
            steps,
            triggers: def.triggers,
        }
    }

    /// Determine the next step after the current step completes with the given output.
    pub fn next_step(
        &self,
        current_step: &str,
        output: &str,
        visit_counts: &mut HashMap<String, u32>,
    ) -> StepAdvance {
        let step_def = match self.steps.get(current_step) {
            Some(s) => s,
            None => return StepAdvance::Escalate,
        };

        // Check transitions on the current step
        for transition in &step_def.transitions {
            if transition_matches(&transition.match_pattern, output) {
                if transition.goto == "escalate" {
                    return StepAdvance::Escalate;
                }
                return self.check_visits(&transition.goto, visit_counts);
            }
        }

        // No transition matched — advance to next in declaration order
        let current_idx = self.steps.get_index_of(current_step).unwrap();
        match self.steps.get_index(current_idx + 1) {
            Some((name, _)) => self.check_visits(name, visit_counts),
            None => StepAdvance::Complete,
        }
    }

    fn check_visits(
        &self,
        target: &str,
        visit_counts: &mut HashMap<String, u32>,
    ) -> StepAdvance {
        let count = visit_counts.entry(target.to_string()).or_insert(0);
        *count += 1;

        if let Some(step_def) = self.steps.get(target) {
            if let Some(max) = step_def.max_visits {
                if *count > max {
                    let goto = step_def
                        .max_visits_goto
                        .as_deref()
                        .unwrap_or("escalate");
                    if goto == "escalate" {
                        return StepAdvance::Escalate;
                    }
                    return StepAdvance::Goto(goto.to_string());
                }
            }
        }

        StepAdvance::Goto(target.to_string())
    }

    /// Get the first step name.
    pub fn first_step(&self) -> Option<&str> {
        self.steps.get_index(0).map(|(name, _)| name.as_str())
    }
}

/// Prefix match with `:` delimiter. `"pass:7"` matches pattern `"pass"`.
/// `"*"` is a catch-all.
pub fn transition_matches(pattern: &str, output: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    output == pattern || output.starts_with(&format!("{pattern}:"))
}

// ── Retry Tracker ───────────────────────────────────────────────────

/// Tracks retry budgets per step. Resets when a different step is dispatched.
#[derive(Debug)]
pub struct RetryTracker {
    counts: HashMap<String, u32>,
    last_step: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum RetryDecision {
    /// Retry the step with the given attempt number.
    Retry { attempt: u32 },
    /// Retries exhausted.
    Exhausted,
}

const DEFAULT_MAX_RETRIES: u32 = 3;

impl RetryTracker {
    pub fn new() -> Self {
        Self {
            counts: HashMap::new(),
            last_step: None,
        }
    }

    /// Record a step failure and decide whether to retry.
    pub fn record_failure(&mut self, step: &str, max_retries: Option<u32>) -> RetryDecision {
        let max = max_retries.unwrap_or(DEFAULT_MAX_RETRIES);

        // Reset if we moved to a different step
        if self.last_step.as_deref() != Some(step) {
            self.counts.clear();
            self.last_step = Some(step.to_string());
        }

        let count = self.counts.entry(step.to_string()).or_insert(0);
        *count += 1;

        if *count <= max {
            RetryDecision::Retry {
                attempt: *count + 1,
            }
        } else {
            RetryDecision::Exhausted
        }
    }

    /// Reset tracker state (e.g. when a step succeeds and we advance).
    pub fn reset(&mut self) {
        self.counts.clear();
        self.last_step = None;
    }
}

// ── Duration serde helper ───────────────────────────────────────────

mod option_duration_secs {
    use serde::{self, Deserialize, Deserializer, Serializer};
    use std::time::Duration;

    pub fn serialize<S>(dur: &Option<Duration>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match dur {
            Some(d) => serializer.serialize_u64(d.as_secs()),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<Duration>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let opt: Option<u64> = Option::deserialize(deserializer)?;
        Ok(opt.map(Duration::from_secs))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_engine() -> WorkflowEngine {
        let def = WorkflowDef {
            name: "test".into(),
            description: String::new(),
            steps: vec![
                StepDef {
                    name: "propose".into(),
                    runtime: None,
                    action: None,
                    output: None,
                    workspace: None,
                    artifacts: vec![],
                    transitions: vec![],
                    max_retries: None,
                    max_visits: Some(3),
                    max_visits_goto: Some("tiebreak".into()),
                    on_fail: None,
                    squash: false,
                },
                StepDef {
                    name: "review".into(),
                    runtime: None,
                    action: None,
                    output: None,
                    workspace: None,
                    artifacts: vec![],
                    transitions: vec![
                        TransitionDef {
                            match_pattern: "pass".into(),
                            goto: "implement".into(),
                        },
                        TransitionDef {
                            match_pattern: "fail".into(),
                            goto: "propose".into(),
                        },
                        TransitionDef {
                            match_pattern: "*".into(),
                            goto: "escalate".into(),
                        },
                    ],
                    max_retries: None,
                    max_visits: None,
                    max_visits_goto: None,
                    on_fail: None,
                    squash: false,
                },
                StepDef {
                    name: "implement".into(),
                    runtime: None,
                    action: None,
                    output: None,
                    workspace: None,
                    artifacts: vec![],
                    transitions: vec![],
                    max_retries: None,
                    max_visits: None,
                    max_visits_goto: None,
                    on_fail: None,
                    squash: false,
                },
                StepDef {
                    name: "tiebreak".into(),
                    runtime: None,
                    action: None,
                    output: None,
                    workspace: None,
                    artifacts: vec![],
                    transitions: vec![],
                    max_retries: None,
                    max_visits: None,
                    max_visits_goto: None,
                    on_fail: None,
                    squash: false,
                },
            ],
            triggers: vec![],
        };
        WorkflowEngine::from_def(def)
    }

    #[test]
    fn transition_prefix_match() {
        assert!(transition_matches("pass", "pass"));
        assert!(transition_matches("pass", "pass:7"));
        assert!(!transition_matches("pass", "passing"));
        assert!(!transition_matches("pass", "fail"));
        assert!(transition_matches("*", "anything"));
    }

    #[test]
    fn advance_via_transition() {
        let engine = make_engine();
        let mut visits = HashMap::new();
        let result = engine.next_step("review", "pass", &mut visits);
        assert_eq!(result, StepAdvance::Goto("implement".into()));
    }

    #[test]
    fn advance_via_declaration_order() {
        let engine = make_engine();
        let mut visits = HashMap::new();
        let result = engine.next_step("propose", "done", &mut visits);
        assert_eq!(result, StepAdvance::Goto("review".into()));
    }

    #[test]
    fn last_step_completes() {
        let engine = make_engine();
        let mut visits = HashMap::new();
        let result = engine.next_step("tiebreak", "done", &mut visits);
        assert_eq!(result, StepAdvance::Complete);
    }

    #[test]
    fn max_visits_triggers_goto() {
        let engine = make_engine();
        let mut visits = HashMap::new();
        visits.insert("propose".into(), 2); // already visited twice

        // Third visit — still ok (max_visits = 3)
        let result = engine.next_step("review", "fail", &mut visits);
        assert_eq!(result, StepAdvance::Goto("propose".into()));

        // Fourth visit — exceeds max_visits, goes to tiebreak
        let result = engine.next_step("review", "fail", &mut visits);
        assert_eq!(result, StepAdvance::Goto("tiebreak".into()));
    }

    #[test]
    fn wildcard_catches_unknown_output() {
        let engine = make_engine();
        let mut visits = HashMap::new();
        let result = engine.next_step("review", "unknown-output", &mut visits);
        // * catch-all goes to "escalate" but escalate isn't a step, so it becomes Escalate
        assert_eq!(result, StepAdvance::Escalate);
    }

    #[test]
    fn first_step() {
        let engine = make_engine();
        assert_eq!(engine.first_step(), Some("propose"));
    }

    #[test]
    fn retry_within_budget() {
        let mut tracker = RetryTracker::new();
        assert_eq!(
            tracker.record_failure("propose", Some(2)),
            RetryDecision::Retry { attempt: 2 }
        );
        assert_eq!(
            tracker.record_failure("propose", Some(2)),
            RetryDecision::Retry { attempt: 3 }
        );
        assert_eq!(
            tracker.record_failure("propose", Some(2)),
            RetryDecision::Exhausted
        );
    }

    #[test]
    fn retry_resets_on_step_change() {
        let mut tracker = RetryTracker::new();
        tracker.record_failure("propose", Some(1));
        // Moving to a different step resets the count
        assert_eq!(
            tracker.record_failure("review", Some(1)),
            RetryDecision::Retry { attempt: 2 }
        );
    }

    #[test]
    fn retry_default_budget() {
        let mut tracker = RetryTracker::new();
        for _ in 0..3 {
            assert!(matches!(
                tracker.record_failure("step", None),
                RetryDecision::Retry { .. }
            ));
        }
        assert_eq!(
            tracker.record_failure("step", None),
            RetryDecision::Exhausted
        );
    }

    #[test]
    fn parse_workflow_toml() {
        let toml = r#"
[workflow]
name = "code-task"
description = "Propose → review → implement → merge"

[[step]]
name = "propose"
output = "diff"
max_visits = 3
max_visits_goto = "tiebreak"

[step.workspace]
git_clone = true
branch = "{task_id}"
push = true

[step.runtime]
type = "claude"
model = "sonnet"
persona = "inspired/engineer"
prompt = "Write a proposal."

[[step]]
name = "review"
output = "verdict"

[[step.transition]]
match = "pass"
goto = "implement"

[[step.transition]]
match = "fail"
goto = "propose"

[[step]]
name = "implement"

[[step]]
name = "merge"
action = "merge_to_main"

[[trigger]]
on = "cx.task_ready"
tag = "workflow:code-task"
workflow = "code-task"
"#;

        let def = WorkflowDef::from_toml(toml).unwrap();
        assert_eq!(def.name, "code-task");
        assert_eq!(def.steps.len(), 4);
        assert_eq!(def.steps[0].name, "propose");
        assert_eq!(def.steps[0].max_visits, Some(3));
        assert!(def.steps[0].runtime.is_some());
        let rt = def.steps[0].runtime.as_ref().unwrap();
        assert_eq!(rt.runtime_type, "claude");
        assert_eq!(def.steps[1].transitions.len(), 2);
        assert_eq!(def.steps[1].transitions[0].match_pattern, "pass");
        assert_eq!(def.steps[3].action.as_deref(), Some("merge_to_main"));
        assert_eq!(def.triggers.len(), 1);
        assert_eq!(def.triggers[0].on, "cx.task_ready");

        // Verify it can be used as engine
        let engine = WorkflowEngine::from_def(def);
        assert_eq!(engine.first_step(), Some("propose"));
    }
}
