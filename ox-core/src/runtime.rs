use anyhow::{Context, Result};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::interpolation::InterpolationContext;
use crate::workflow::{RuntimeSpec, VarDef};

/// A runtime definition, loaded from TOML.
/// Defines how to build a command, what files to place, what env to set, etc.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeDef {
    pub name: String,
    #[serde(default)]
    pub vars: IndexMap<String, VarDef>,
    pub command: CommandDef,
    #[serde(default)]
    pub files: Vec<FileMappingDef>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub proxy: Vec<ProxyDef>,
    #[serde(default)]
    pub metrics: Vec<MetricDef>,
}

/// TOML file layout: [runtime] header.
#[derive(Debug, Deserialize)]
struct RuntimeFile {
    runtime: RuntimeDef,
}

impl RuntimeDef {
    /// Parse a runtime definition from a TOML string.
    pub fn from_toml(toml_str: &str) -> Result<Self, toml::de::Error> {
        let file: RuntimeFile = toml::from_str(toml_str)?;
        Ok(file.runtime)
    }

    /// Load a runtime definition from a TOML file.
    pub fn from_file(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("reading runtime file: {}", path.display()))?;
        Self::from_toml(&content).with_context(|| format!("parsing runtime: {}", path.display()))
    }
}

/// Command definition — base command + optional conditional args.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandDef {
    pub cmd: Vec<String>,
    #[serde(default)]
    pub interactive_cmd: Option<Vec<String>>,
    #[serde(default)]
    pub optional: Vec<OptionalArgsDef>,
}

/// Conditional arguments appended when a field has a value.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OptionalArgsDef {
    pub when: String,
    pub args: Vec<String>,
}

/// File placement rule — copy a file into the workspace before runtime starts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileMappingDef {
    /// Source file path (resolved via search path for `file` type fields).
    /// Mutually exclusive with `content`.
    #[serde(default)]
    pub from: Option<String>,
    /// Inline content (used with secrets). Mutually exclusive with `from`.
    #[serde(default)]
    pub content: Option<String>,
    /// Destination path, typically relative to `{workspace}`.
    pub to: String,
    /// POSIX permission string (default: "0644").
    #[serde(default = "default_file_mode")]
    pub mode: String,
}

fn default_file_mode() -> String {
    "0644".into()
}

/// API proxy declaration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyDef {
    /// Environment variable to override with proxy address.
    pub env: String,
    /// API provider format (e.g. "anthropic", "openai").
    pub provider: String,
    /// Upstream URL the proxy forwards requests to.
    pub target: String,
}

/// Metric declaration in a runtime definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricDef {
    pub name: String,
    #[serde(rename = "type")]
    pub metric_type: MetricType,
    #[serde(default = "default_metric_source")]
    pub source: MetricSource,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MetricType {
    Gauge,
    Counter,
    Histogram,
    Label,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MetricSource {
    Runtime,
    Proxy,
}

fn default_metric_source() -> MetricSource {
    MetricSource::Runtime
}

/// The fully-resolved step spec that ox-server sends to the runner.
/// Contains everything the runner needs to execute a step — no config lookup required.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedStepSpec {
    /// The command to spawn.
    pub command: Vec<String>,
    /// Alternate command for TTY mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interactive_command: Option<Vec<String>>,
    /// Whether to allocate a TTY.
    #[serde(default)]
    pub tty: bool,
    /// Environment variables to set on the spawned process.
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Files to place in the workspace before spawning.
    #[serde(default)]
    pub files: Vec<ResolvedFile>,
    /// API proxy declarations.
    #[serde(default)]
    pub proxy: Vec<ProxyDef>,
    /// Metric declarations.
    #[serde(default)]
    pub metrics: Vec<MetricDef>,
}

/// A file to place before the runtime runs.
///
/// The `to` path uses placeholders the runner resolves:
///   - `{workspace}/...` — relative to the step workspace (work_dir)
///   - `{tmp_dir}/...`   — runner's temp directory (not in git)
///   - `{home}/...`      — runner's HOME directory
///   - bare name         — placed in tmp_dir by default
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedFile {
    /// The file content.
    pub content: String,
    /// Destination path with placeholders ({workspace}, {tmp_dir}, {home}).
    pub to: String,
    /// POSIX permission string.
    #[serde(default = "default_file_mode")]
    pub mode: String,
}

// ── Resolution ──────────────────────────────────────────────────────

/// Resolve a workflow step's runtime spec against a runtime definition,
/// producing a fully-resolved spec the runner can execute.
pub fn resolve_step_spec(
    runtime_def: &RuntimeDef,
    step_runtime: &RuntimeSpec,
    secrets: &HashMap<String, String>,
    _search_path: &[PathBuf],
    context_vars: &HashMap<String, String>,
) -> Result<ResolvedStepSpec> {
    // 1. Build field values: runtime vars prefixed with "var.", context vars as-is
    let mut field_values: HashMap<String, String> = HashMap::new();

    // Runtime vars: prefixed with "var." (e.g. {var.prompt}, {var.model})
    for (name, def) in &runtime_def.vars {
        let key = format!("var.{name}");
        if let Some(val) = step_runtime.fields.get(name) {
            field_values.insert(key, toml_value_to_string(val));
        } else if let Some(ref default) = def.default {
            field_values.insert(key, default.clone());
        }
    }

    // Context vars: workflow.* and builtins (workspace, etc.) — already prefixed by caller
    for (k, v) in context_vars {
        field_values.insert(k.clone(), v.clone());
    }

    let mut resolved_files: Vec<ResolvedFile> = vec![];

    // 2. Build interpolation context
    let ctx = InterpolationContext::new(field_values.clone(), secrets.clone());

    // 3. Resolve content-based file mappings (prompt assembly, credentials, etc.)
    // Two-pass interpolation: first pass resolves the template (e.g. {workflow.persona}
    // expands to file content), second pass resolves references inside that content
    // (e.g. {workflow.task_id} inside a persona file).
    for file_mapping in &runtime_def.files {
        if let Some(ref content_template) = file_mapping.content
            && let Ok(content) = ctx.interpolate(content_template) {
                let content = ctx.interpolate(&content).unwrap_or(content);
                let to = ctx.interpolate(&file_mapping.to)
                    .unwrap_or_else(|_| file_mapping.to.clone());
                resolved_files.push(ResolvedFile {
                    content,
                    to,
                    mode: file_mapping.mode.clone(),
                });
            }
    }

    // 5. Resolve command
    let base_cmd = if step_runtime.tty {
        runtime_def.command.interactive_cmd.clone().unwrap_or_else(|| runtime_def.command.cmd.clone())
    } else {
        runtime_def.command.cmd.clone()
    };

    let mut command: Vec<String> = vec![];
    for arg in &base_cmd {
        command.push(ctx.interpolate(arg).unwrap_or_else(|_| arg.clone()));
    }

    // Append optional args where the runtime var has a value
    for opt in &runtime_def.command.optional {
        if ctx.has_field(&format!("var.{}", opt.when)) {
            for arg in &opt.args {
                command.push(ctx.interpolate(arg).unwrap_or_else(|_| arg.clone()));
            }
        }
    }

    // 6. Resolve env vars
    let mut env: HashMap<String, String> = HashMap::new();
    for (k, v) in &runtime_def.env {
        if let Ok(resolved) = ctx.interpolate(v) {
            env.insert(k.clone(), resolved);
        }
    }
    for (k, v) in &step_runtime.env {
        if let Ok(resolved) = ctx.interpolate(v) {
            env.insert(k.clone(), resolved);
        }
    }

    // 7. Collect secret refs for audit
    let _secret_refs = collect_secret_refs(runtime_def, step_runtime);

    Ok(ResolvedStepSpec {
        command,
        interactive_command: if step_runtime.tty {
            None // already used as the main command
        } else {
            runtime_def.command.interactive_cmd.clone()
        },
        tty: step_runtime.tty,
        env,
        files: resolved_files,
        proxy: runtime_def.proxy.clone(),
        metrics: runtime_def.metrics.clone(),
    })
}

/// Collect all secret names referenced by a runtime def + step spec.
pub fn collect_secret_refs(runtime_def: &RuntimeDef, step_runtime: &RuntimeSpec) -> Vec<String> {
    let templates: Vec<&str> = runtime_def
        .env
        .values()
        .map(|v| v.as_str())
        .chain(step_runtime.env.values().map(|v| v.as_str()))
        .collect();
    InterpolationContext::collect_all_secret_refs(templates)
}

pub fn toml_value_to_string(v: &toml::Value) -> String {
    match v {
        toml::Value::String(s) => s.clone(),
        toml::Value::Integer(i) => i.to_string(),
        toml::Value::Float(f) => f.to_string(),
        toml::Value::Boolean(b) => b.to_string(),
        other => other.to_string(),
    }
}

/// Find a file by name in the search path under a subdirectory.
/// Tries exact name and .md extension.
/// Tries with and without common extensions (.md, .toml, .txt).
pub fn find_and_read_file(search_path: &[PathBuf], subdir: &str, name: &str) -> Option<String> {
    for dir in search_path {
        let base = dir.join(subdir);
        // Try exact name
        let candidate = base.join(name);
        if candidate.is_file() {
            return std::fs::read_to_string(&candidate).ok();
        }
        // Try with .md extension
        let candidate = base.join(format!("{name}.md"));
        if candidate.is_file() {
            return std::fs::read_to_string(&candidate).ok();
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn var_type_serde() {
        use crate::workflow::VarType;
        let vt = VarType::File;
        let json = serde_json::to_string(&vt).unwrap();
        assert_eq!(json, "\"file\"");
    }

    #[test]
    fn metric_source_default() {
        assert_eq!(default_metric_source(), MetricSource::Runtime);
    }

    #[test]
    fn resolve_step_spec_scoped_vars() {
        use crate::workflow::{RuntimeSpec, VarDef, VarType};

        // Runtime with a "prompt" var
        let runtime_def = RuntimeDef {
            name: "test".into(),
            vars: IndexMap::from([(
                "prompt".into(),
                VarDef {
                    var_type: VarType::String,
                    required: false,
                    default: Some("default-prompt".into()),
                    description: None,
                    search_dir: None,
                },
            )]),
            command: CommandDef {
                cmd: vec!["echo".into(), "{var.prompt}".into()],
                interactive_cmd: None,
                optional: vec![],
            },
            files: vec![
                FileMappingDef {
                    from: None,
                    content: Some("{workflow.task_id} says {var.prompt}".into()),
                    to: "{tmp_dir}/test.txt".into(),
                    mode: "0644".into(),
                },
            ],
            env: HashMap::new(),
            proxy: vec![],
            metrics: vec![],
        };

        let step_runtime = RuntimeSpec {
            runtime: "test".into(),
            tty: false,
            env: HashMap::new(),
            timeout: None,
            fields: HashMap::new(),
        };

        // Context vars (workflow vars already prefixed by dispatch handler)
        let mut context_vars = HashMap::new();
        context_vars.insert("workflow.task_id".into(), "aJuO".into());

        let resolved = resolve_step_spec(
            &runtime_def,
            &step_runtime,
            &HashMap::new(),
            &[],
            &context_vars,
        )
        .unwrap();

        // Command should resolve {var.prompt} to the default
        assert_eq!(resolved.command, vec!["echo", "default-prompt"]);

        // File content should resolve both scopes
        assert_eq!(resolved.files.len(), 1);
        assert_eq!(resolved.files[0].content, "aJuO says default-prompt");
    }

    #[test]
    fn resolve_step_spec_step_override() {
        use crate::workflow::{RuntimeSpec, VarDef, VarType};

        let runtime_def = RuntimeDef {
            name: "test".into(),
            vars: IndexMap::from([(
                "prompt".into(),
                VarDef {
                    var_type: VarType::String,
                    required: false,
                    default: Some("default".into()),
                    description: None,
                    search_dir: None,
                },
            )]),
            command: CommandDef {
                cmd: vec!["echo".into(), "{var.prompt}".into()],
                interactive_cmd: None,
                optional: vec![],
            },
            files: vec![],
            env: HashMap::new(),
            proxy: vec![],
            metrics: vec![],
        };

        // Step overrides prompt
        let mut fields = HashMap::new();
        fields.insert("prompt".into(), toml::Value::String("overridden".into()));

        let step_runtime = RuntimeSpec {
            runtime: "test".into(),
            tty: false,
            env: HashMap::new(),
            timeout: None,
            fields,
        };

        let resolved = resolve_step_spec(
            &runtime_def,
            &step_runtime,
            &HashMap::new(),
            &[],
            &HashMap::new(),
        )
        .unwrap();

        assert_eq!(resolved.command, vec!["echo", "overridden"]);
    }
}
