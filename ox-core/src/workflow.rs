use crate::events::EventEnvelope;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;
use thiserror::Error;

/// Variable type — shared by workflow vars and runtime vars.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VarType {
    #[default]
    String,
    File,
    Bool,
    Int,
}

/// A variable declaration — shared by workflow vars and runtime vars.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VarDef {
    #[serde(rename = "type", default)]
    pub var_type: VarType,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub default: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// For file vars: subdirectory to search on the search path.
    /// Defaults to "{varname}s" (e.g. "personas" for a var named "persona").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search_dir: Option<String>,
}

/// A complete workflow definition, loaded from TOML.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowDef {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Default persona for steps that don't specify their own.
    #[serde(default)]
    pub persona: Option<String>,
    /// Workflow-level skills, added to every step.
    #[serde(default)]
    pub skills: Vec<String>,
    #[serde(default)]
    pub vars: HashMap<String, VarDef>,
    #[serde(default, rename = "step")]
    pub steps: Vec<StepDef>,
}

/// TOML file layout: [workflow] header + [[step]] arrays.
#[derive(Debug, Deserialize)]
struct WorkflowFile {
    workflow: WorkflowHeader,
    #[serde(default)]
    step: Vec<StepDef>,
}

#[derive(Debug, Deserialize)]
struct WorkflowHeader {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    persona: Option<String>,
    #[serde(default)]
    skills: Vec<String>,
    #[serde(default)]
    vars: HashMap<String, VarDef>,
}

impl WorkflowDef {
    /// Parse a workflow definition from a TOML string.
    pub fn from_toml(toml_str: &str) -> Result<Self, toml::de::Error> {
        let file: WorkflowFile = toml::from_str(toml_str)?;
        Ok(Self {
            name: file.workflow.name,
            description: file.workflow.description,
            persona: file.workflow.persona,
            skills: file.workflow.skills,
            vars: file.workflow.vars,
            steps: file.step,
        })
    }

    /// Load a workflow definition from a TOML file.
    pub fn from_file(path: &Path) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        Ok(Self::from_toml(&content)?)
    }

    /// Validate caller-provided vars against declarations.
    ///
    /// Returns the merged variable map: caller values + defaults for
    /// omitted optional vars. Returns an error if a required var is missing.
    pub fn validate_vars(
        &self,
        input: &HashMap<String, String>,
    ) -> Result<HashMap<String, String>, String> {
        let mut merged = input.clone();

        for (name, def) in &self.vars {
            if merged.contains_key(name) {
                continue;
            }
            if let Some(ref default) = def.default {
                merged.insert(name.clone(), default.clone());
            } else if def.required {
                return Err(format!("missing required variable: {name}"));
            }
        }

        Ok(merged)
    }
}

/// A single step in a workflow.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepDef {
    pub name: String,
    /// Persona for this step. Overrides the workflow default.
    #[serde(default)]
    pub persona: Option<String>,
    /// Step-level prompt passed to the runtime.
    #[serde(default)]
    pub prompt: Option<String>,
    /// Additional skills for this step (added to runtime + persona + workflow skills).
    #[serde(default)]
    pub skills: Vec<String>,
    /// Runtime override — escape hatch for overriding persona's runtime/model.
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
/// In persona-primary mode, `runtime` is optional — the persona declares it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeSpec {
    #[serde(default)]
    pub runtime: String,
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
    /// Event kind to match. For source events this is the watcher-native
    /// `kind` (`node.ready`, `issue.labeled`, ...); for legacy cx events
    /// this is the dotted event type (`cx.task_ready`).
    pub on: String,
    /// Watcher identifier to filter on. Only source events from this
    /// watcher match. `None` means any source. Legacy cx-flavored
    /// triggers leave this unset.
    #[serde(default)]
    pub source: Option<String>,
    /// Generic field predicates. Keys are event paths without the
    /// `event.` prefix, e.g. `data.tags` or `data.workflow`.
    #[serde(default, rename = "where")]
    pub where_: HashMap<String, TriggerWhere>,
    pub workflow: String,
    #[serde(default)]
    pub poll_interval: Option<String>,
    /// Mapping from workflow var name to a template interpolated against
    /// `{event.*}` fields of the firing event context.
    #[serde(default)]
    pub vars: HashMap<String, String>,
}

/// Errors returned when a trigger cannot produce workflow vars from an event.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TriggerError {
    #[error("missing event field: {path}")]
    MissingEventField { path: String },
}

impl TriggerDef {
    /// Return true when every `[trigger.where]` predicate matches the
    /// firing event envelope.
    pub fn matches_where(&self, envelope: &EventEnvelope) -> bool {
        self.where_.iter().all(|(path, cond)| {
            let event_path = format!("event.{path}");
            cond.matches(envelope.resolve_value(&event_path))
        })
    }

    /// Resolve the trigger's `[trigger.vars]` block against an event envelope,
    /// returning the map of workflow vars to pass to `create_execution`.
    ///
    /// Each value is a template that may reference `{event.X}` fields from
    /// the firing event. Unknown fields produce `TriggerError::MissingEventField`.
    pub fn build_vars(
        &self,
        envelope: &EventEnvelope,
    ) -> Result<HashMap<String, String>, TriggerError> {
        let mut out = HashMap::with_capacity(self.vars.len());
        for (name, template) in &self.vars {
            out.insert(name.clone(), interpolate_event_template(template, envelope)?);
        }
        Ok(out)
    }
}

/// A single `[trigger.where]` condition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TriggerWhere {
    /// Exact scalar match.
    Eq(String),
    /// Array/string containment.
    Contains { contains: String },
}

impl TriggerWhere {
    fn matches(&self, value: Option<serde_json::Value>) -> bool {
        let Some(value) = value else {
            return false;
        };

        match self {
            Self::Eq(want) => match value {
                serde_json::Value::String(s) => s == *want,
                serde_json::Value::Number(n) => n.to_string() == *want,
                serde_json::Value::Bool(b) => b.to_string() == *want,
                _ => false,
            },
            Self::Contains { contains } => match value {
                serde_json::Value::Array(items) => items.iter().any(|item| match item {
                    serde_json::Value::String(s) => s == contains,
                    serde_json::Value::Number(n) => n.to_string() == *contains,
                    serde_json::Value::Bool(b) => b.to_string() == *contains,
                    _ => false,
                }),
                serde_json::Value::String(s) => s == *contains,
                _ => false,
            },
        }
    }
}

/// Interpolate `{event.X}` references in a template against the given envelope.
/// Literal text and non-event braces are passed through unchanged.
fn interpolate_event_template(
    template: &str,
    envelope: &EventEnvelope,
) -> Result<String, TriggerError> {
    let mut out = String::with_capacity(template.len());
    let mut chars = template.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '{' {
            let path: String = chars.by_ref().take_while(|&c| c != '}').collect();
            if path.starts_with("event.") {
                match envelope.resolve(&path) {
                    Some(value) => out.push_str(&value),
                    None => return Err(TriggerError::MissingEventField { path }),
                }
            } else {
                // Non-event references are left intact — downstream interpolation
                // (runtime, step prompt) resolves them later against workflow vars.
                out.push('{');
                out.push_str(&path);
                out.push('}');
            }
        } else {
            out.push(ch);
        }
    }
    Ok(out)
}

/// TOML layout for a standalone triggers file: [[trigger]] arrays.
#[derive(Debug, Deserialize)]
pub struct TriggersFile {
    #[serde(default)]
    pub trigger: Vec<TriggerDef>,
}

// ── Workflow Engine ─────────────────────────────────────────────────

/// Step graph indexed by name for O(1) lookup with preserved declaration order.
pub struct WorkflowEngine {
    pub name: String,
    /// Default persona for steps that don't specify their own.
    pub persona: Option<String>,
    /// Workflow-level skills.
    pub skills: Vec<String>,
    pub vars: HashMap<String, VarDef>,
    pub steps: IndexMap<String, StepDef>,
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
        let vars = def.vars;
        let mut steps = IndexMap::new();
        for step in def.steps {
            steps.insert(step.name.clone(), step);
        }
        Self {
            name: def.name,
            persona: def.persona,
            skills: def.skills,
            vars,
            steps,
        }
    }

    /// Validate caller-provided vars against declarations.
    /// See [`WorkflowDef::validate_vars`] for details.
    pub fn validate_vars(
        &self,
        input: &HashMap<String, String>,
    ) -> Result<HashMap<String, String>, String> {
        let mut merged = input.clone();
        for (name, def) in &self.vars {
            if merged.contains_key(name) {
                continue;
            }
            if let Some(ref default) = def.default {
                merged.insert(name.clone(), default.clone());
            } else if def.required {
                return Err(format!("missing required variable: {name}"));
            }
        }
        Ok(merged)
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
                if let Some(advance) = reserved_step_advance(&transition.goto) {
                    return advance;
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

    fn check_visits(&self, target: &str, visit_counts: &mut HashMap<String, u32>) -> StepAdvance {
        let count = visit_counts.entry(target.to_string()).or_insert(0);
        *count += 1;

        if let Some(step_def) = self.steps.get(target)
            && let Some(max) = step_def.max_visits
            && *count > max
        {
            let goto = step_def.max_visits_goto.as_deref().unwrap_or("escalate");
            if let Some(advance) = reserved_step_advance(goto) {
                return advance;
            }
            return StepAdvance::Goto(goto.to_string());
        }

        StepAdvance::Goto(target.to_string())
    }

    /// Get the first step name.
    pub fn first_step(&self) -> Option<&str> {
        self.steps.get_index(0).map(|(name, _)| name.as_str())
    }
}

fn reserved_step_advance(target: &str) -> Option<StepAdvance> {
    match target {
        "complete" => Some(StepAdvance::Complete),
        "escalate" => Some(StepAdvance::Escalate),
        _ => None,
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

impl Default for RetryTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl RetryTracker {
    pub fn new() -> Self {
        Self {
            counts: HashMap::new(),
            last_step: None,
        }
    }

    /// Record a step failure and decide whether to retry.
    ///
    /// `force_escalate = true` makes the tracker return `Exhausted`
    /// regardless of the remaining retry budget — the runner has
    /// reported a non-retriable failure signal and burning more
    /// attempts wouldn't change the outcome. The attempt counter is
    /// also cleared so a manual rerun starts fresh.
    pub fn record_failure(
        &mut self,
        step: &str,
        max_retries: Option<u32>,
        force_escalate: bool,
    ) -> RetryDecision {
        if force_escalate {
            // Clear the counter so a manual rerun starts at attempt 1
            // again — the prior attempts shouldn't count against a
            // human re-trying after fixing the underlying cause
            // (e.g. rotating credentials).
            self.counts.remove(step);
            self.last_step = Some(step.to_string());
            return RetryDecision::Exhausted;
        }

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

    fn make_step(name: &str) -> StepDef {
        StepDef {
            name: name.into(),
            persona: None,
            prompt: None,
            skills: vec![],
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
        }
    }

    fn make_engine() -> WorkflowEngine {
        let def = WorkflowDef {
            name: "test".into(),
            description: String::new(),
            persona: None,
            skills: vec![],
            vars: HashMap::new(),
            steps: vec![
                StepDef {
                    max_visits: Some(3),
                    max_visits_goto: Some("tiebreak".into()),
                    ..make_step("propose")
                },
                StepDef {
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
                    ..make_step("review")
                },
                make_step("implement"),
                make_step("tiebreak"),
            ],
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
    fn transition_can_complete_execution() {
        let mut engine = make_engine();
        engine.steps.get_mut("review").unwrap().transitions.insert(
            0,
            TransitionDef {
                match_pattern: "none".into(),
                goto: "complete".into(),
            },
        );

        let mut visits = HashMap::new();
        let result = engine.next_step("review", "none", &mut visits);

        assert_eq!(result, StepAdvance::Complete);
        assert!(
            !visits.contains_key("complete"),
            "reserved terminal target should not be counted as a step visit"
        );
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
            tracker.record_failure("propose", Some(2), false),
            RetryDecision::Retry { attempt: 2 }
        );
        assert_eq!(
            tracker.record_failure("propose", Some(2), false),
            RetryDecision::Retry { attempt: 3 }
        );
        assert_eq!(
            tracker.record_failure("propose", Some(2), false),
            RetryDecision::Exhausted
        );
    }

    #[test]
    fn retry_resets_on_step_change() {
        let mut tracker = RetryTracker::new();
        tracker.record_failure("propose", Some(1), false);
        // Moving to a different step resets the count
        assert_eq!(
            tracker.record_failure("review", Some(1), false),
            RetryDecision::Retry { attempt: 2 }
        );
    }

    #[test]
    fn retry_default_budget() {
        let mut tracker = RetryTracker::new();
        for _ in 0..3 {
            assert!(matches!(
                tracker.record_failure("step", None, false),
                RetryDecision::Retry { .. }
            ));
        }
        assert_eq!(
            tracker.record_failure("step", None, false),
            RetryDecision::Exhausted
        );
    }

    #[test]
    fn force_escalate_exhausts_immediately_and_clears_counter() {
        let mut tracker = RetryTracker::new();
        // First failure with force_escalate=true must skip the budget.
        assert_eq!(
            tracker.record_failure("propose", Some(5), true),
            RetryDecision::Exhausted,
            "force_escalate=true must return Exhausted on the first failure"
        );
        // Counter must be cleared so a manual rerun isn't poisoned —
        // the next failure on the same step starts at attempt 2 again.
        assert_eq!(
            tracker.record_failure("propose", Some(5), false),
            RetryDecision::Retry { attempt: 2 },
            "force_escalate must reset the counter so reruns aren't poisoned"
        );
    }

    #[test]
    fn parse_workflow_toml() {
        let toml = r#"
[workflow]
name = "code-task"
description = "Propose → review → implement → merge"

[workflow.vars]
task_id = { required = true, description = "cx task identifier" }

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
runtime = "claude"
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
"#;

        let def = WorkflowDef::from_toml(toml).unwrap();
        assert_eq!(def.name, "code-task");
        assert_eq!(def.steps.len(), 4);
        assert_eq!(def.steps[0].name, "propose");
        assert_eq!(def.steps[0].max_visits, Some(3));
        assert!(def.steps[0].runtime.is_some());
        let rt = def.steps[0].runtime.as_ref().unwrap();
        assert_eq!(rt.runtime, "claude");
        assert_eq!(def.steps[1].transitions.len(), 2);
        assert_eq!(def.steps[1].transitions[0].match_pattern, "pass");
        assert_eq!(def.steps[3].action.as_deref(), Some("merge_to_main"));

        // Verify vars parsed
        assert!(def.vars.contains_key("task_id"));
        assert!(def.vars["task_id"].required);

        // Verify it can be used as engine
        let engine = WorkflowEngine::from_def(def);
        assert_eq!(engine.first_step(), Some("propose"));
    }

    #[test]
    fn validate_vars_required() {
        let mut vars = HashMap::new();
        vars.insert("task_id".into(), VarDef {
            var_type: VarType::default(),
            required: true,
            default: None,
            description: None,
            search_dir: None,
        });
        let def = WorkflowDef {
            name: "test".into(),
            description: String::new(),
            persona: None,
            skills: vec![],
            vars,
            steps: vec![],
        };

        // Missing required var
        let result = def.validate_vars(&HashMap::new());
        assert!(result.is_err());

        // Provided
        let mut input = HashMap::new();
        input.insert("task_id".into(), "abc".into());
        let result = def.validate_vars(&input).unwrap();
        assert_eq!(result["task_id"], "abc");
    }

    #[test]
    fn validate_vars_defaults() {
        let mut vars = HashMap::new();
        vars.insert("branch".into(), VarDef {
            var_type: VarType::default(),
            required: false,
            default: Some("main".into()),
            description: None,
            search_dir: None,
        });
        let def = WorkflowDef {
            name: "test".into(),
            description: String::new(),
            persona: None,
            skills: vec![],
            vars,
            steps: vec![],
        };

        // No input — default fills in
        let result = def.validate_vars(&HashMap::new()).unwrap();
        assert_eq!(result["branch"], "main");

        // Caller overrides default
        let mut input = HashMap::new();
        input.insert("branch".into(), "feature-x".into());
        let result = def.validate_vars(&input).unwrap();
        assert_eq!(result["branch"], "feature-x");
    }

    #[test]
    fn tty_flag_survives_toml_to_json_round_trip() {
        let toml = r#"
[workflow]
name = "interactive"
description = "Interactive shell"

[[step]]
name = "shell"

[step.runtime]
type = "shell"
tty = true

[step.workspace]
git_clone = true
branch = "{task_id}"
push = true
"#;

        let def = WorkflowDef::from_toml(toml).unwrap();
        let rt = def.steps[0].runtime.as_ref().unwrap();
        assert!(rt.tty, "tty should be true after TOML parse");

        // Simulate what the herder does: serialize RuntimeSpec to JSON
        let json = serde_json::to_value(rt).unwrap();
        assert_eq!(json.get("tty").and_then(|v| v.as_bool()), Some(true),
            "tty should be true in JSON: {json}");

        // Simulate what the server does: deserialize back to RuntimeSpec
        let rt2: RuntimeSpec = serde_json::from_value(json).unwrap();
        assert!(rt2.tty, "tty should be true after JSON round-trip");
    }

    #[test]
    fn parse_triggers_file() {
        let toml = r#"
[[trigger]]
on     = "node.ready"
source = "cx"
workflow = "code-task"
[trigger.where]
"data.tags" = { contains = "workflow:code-task" }

[[trigger]]
on     = "comment.added"
source = "cx"
workflow = "code-task"
[trigger.where]
"data.tag" = "review-requested"
"#;
        let file: TriggersFile = toml::from_str(toml).unwrap();
        assert_eq!(file.trigger.len(), 2);
        assert_eq!(file.trigger[0].on, "node.ready");
        assert_eq!(file.trigger[0].source.as_deref(), Some("cx"));
        assert_eq!(file.trigger[0].workflow, "code-task");
        assert_eq!(
            file.trigger[0].where_.get("data.tags"),
            Some(&TriggerWhere::Contains {
                contains: "workflow:code-task".into()
            })
        );
        assert_eq!(file.trigger[1].on, "comment.added");
        assert_eq!(
            file.trigger[1].where_.get("data.tag"),
            Some(&TriggerWhere::Eq("review-requested".into()))
        );
    }

    #[test]
    fn parse_trigger_with_vars_block() {
        let toml = r#"
[[trigger]]
on       = "node.ready"
source   = "cx"
workflow = "consultation"
[trigger.where]
"data.tags" = { contains = "workflow:consultation" }
[trigger.vars]
branch = "{event.subject_id}"
"#;
        let file: TriggersFile = toml::from_str(toml).unwrap();
        assert_eq!(file.trigger.len(), 1);
        let t = &file.trigger[0];
        assert_eq!(
            t.vars.get("branch").map(String::as_str),
            Some("{event.subject_id}")
        );
        assert!(t.matches_where(&sample_envelope(
            "Q6cY",
            serde_json::json!({ "tags": ["workflow:consultation"] }),
        )));
        assert!(!t.matches_where(&sample_envelope(
            "Q6cY",
            serde_json::json!({ "tags": ["workflow:other"] }),
        )));
    }

    fn sample_envelope(subject: &str, data: serde_json::Value) -> EventEnvelope {
        EventEnvelope {
            seq: crate::types::Seq(1),
            ts: chrono::Utc::now(),
            source: "cx".into(),
            kind: "node.ready".into(),
            subject_id: subject.into(),
            data,
        }
    }

    fn sample_source_ctx(subject: &str) -> EventEnvelope {
        sample_envelope(
            subject,
            serde_json::json!({
                "tags": ["workflow:code-task"],
                "state": "ready"
            }),
        )
    }

    #[test]
    fn build_vars_interpolates_subject_id_into_named_var() {
        // The consultation workflow uses `branch` as its var name.
        // Its trigger maps event.subject_id → branch.
        let trigger = TriggerDef {
            on: "node.ready".into(),
            source: Some("cx".into()),
            where_: HashMap::new(),
            workflow: "consultation".into(),
            poll_interval: None,
            vars: HashMap::from([("branch".into(), "{event.subject_id}".into())]),
        };
        let ctx = sample_source_ctx("aJuO");

        let result = trigger.build_vars(&ctx).expect("build_vars should succeed");

        assert_eq!(result.get("branch").map(String::as_str), Some("aJuO"));
        assert!(
            !result.contains_key("task_id"),
            "consultation trigger must NOT silently populate task_id"
        );
    }

    #[test]
    fn build_vars_interpolates_for_code_task_shape() {
        // The code-task workflow uses `task_id` as its var name.
        let trigger = TriggerDef {
            on: "node.ready".into(),
            source: Some("cx".into()),
            where_: HashMap::new(),
            workflow: "code-task".into(),
            poll_interval: None,
            vars: HashMap::from([("task_id".into(), "{event.subject_id}".into())]),
        };
        let ctx = sample_source_ctx("bXYz");

        let result = trigger.build_vars(&ctx).unwrap();

        assert_eq!(result.get("task_id").map(String::as_str), Some("bXYz"));
    }

    #[test]
    fn build_vars_errors_on_unknown_event_field() {
        let trigger = TriggerDef {
            on: "node.ready".into(),
            source: None,
            where_: HashMap::new(),
            workflow: "whatever".into(),
            poll_interval: None,
            vars: HashMap::from([("x".into(), "{event.bogus}".into())]),
        };
        let ctx = sample_source_ctx("n");

        let err = trigger
            .build_vars(&ctx)
            .expect_err("should fail on bogus field");

        assert_eq!(
            err,
            TriggerError::MissingEventField {
                path: "event.bogus".into()
            }
        );
    }

    #[test]
    fn build_vars_empty_block_returns_empty_map() {
        // A trigger with no [trigger.vars] block should produce no vars —
        // NOT magically inject task_id or anything else.
        let trigger = TriggerDef {
            on: "node.ready".into(),
            source: None,
            where_: HashMap::new(),
            workflow: "whatever".into(),
            poll_interval: None,
            vars: HashMap::new(),
        };
        let ctx = sample_source_ctx("n");

        let result = trigger.build_vars(&ctx).unwrap();
        assert!(result.is_empty());
    }
}
