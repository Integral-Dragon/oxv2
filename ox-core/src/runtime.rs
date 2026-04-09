use anyhow::{Context, Result};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::interpolation::InterpolationContext;
use crate::workflow::RuntimeSpec;

/// A runtime definition, loaded from TOML.
/// Defines how to build a command, what files to place, what env to set, etc.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeDef {
    pub name: String,
    #[serde(default)]
    pub fields: IndexMap<String, FieldDef>,
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

/// A declared field in a runtime definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FieldDef {
    #[serde(rename = "type")]
    pub field_type: FieldType,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub default: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FieldType {
    String,
    File,
    Bool,
    Int,
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

const PROMPT_FILE_NAME: &str = "ox-prompt";

/// Resolve a workflow step's runtime spec against a runtime definition,
/// producing a fully-resolved spec the runner can execute.
pub fn resolve_step_spec(
    runtime_def: &RuntimeDef,
    step_runtime: &RuntimeSpec,
    secrets: &HashMap<String, String>,
    search_path: &[PathBuf],
    context_vars: &HashMap<String, String>,
) -> Result<ResolvedStepSpec> {
    // 1. Build field values: defaults from RuntimeDef, overridden by step fields
    let mut field_values: HashMap<String, String> = HashMap::new();

    for (name, def) in &runtime_def.fields {
        // Step field override
        if let Some(val) = step_runtime.fields.get(name) {
            field_values.insert(name.clone(), toml_value_to_string(val));
        } else if let Some(ref default) = def.default {
            field_values.insert(name.clone(), default.clone());
        }
    }

    // Add context vars (task_id, workspace, etc.)
    for (k, v) in context_vars {
        field_values.insert(k.clone(), v.clone());
    }

    // 2. Load file-type fields (e.g. persona) and resolve their content
    let mut resolved_files: Vec<ResolvedFile> = vec![];

    for (name, def) in &runtime_def.fields {
        if def.field_type == FieldType::File {
            if let Some(file_ref) = field_values.get(name).cloned() {
                if !file_ref.is_empty() {
                    if let Some(content) = find_and_read_file(search_path, "personas", &file_ref) {
                        // Store the loaded content so it can be used (e.g. prepended to prompt)
                        field_values.insert(format!("{name}_content"), content);
                    }
                }
            }
        }
    }

    // 3. Build interpolation context (needed for prompt assembly and everything after)
    let ctx = InterpolationContext::new(field_values.clone(), secrets.clone());

    // 4. Assemble prompt file: persona content + step prompt, with interpolation
    {
        let persona_content = field_values.get("persona_content").cloned().unwrap_or_default();
        let prompt = field_values.get("prompt").cloned().unwrap_or_default();

        if !persona_content.is_empty() || !prompt.is_empty() {
            let mut full_prompt = String::new();
            if !persona_content.is_empty() {
                // Interpolate {task_id} etc. in persona content
                let resolved = ctx.interpolate(&persona_content)
                    .unwrap_or(persona_content);
                full_prompt.push_str(&resolved);
                full_prompt.push_str("\n\n---\n\n");
            }
            if !prompt.is_empty() {
                // Interpolate {task_id} etc. in step prompt
                let resolved = ctx.interpolate(&prompt)
                    .unwrap_or(prompt);
                full_prompt.push_str(&resolved);
            }

            let prompt_path = format!("{{tmp_dir}}/{PROMPT_FILE_NAME}");
            resolved_files.push(ResolvedFile {
                content: full_prompt,
                to: prompt_path.clone(),
                mode: "0644".to_string(),
            });
            field_values.insert("prompt_file".to_string(), prompt_path);
        }
    }

    // Rebuild context now that prompt_file is set
    let ctx = InterpolationContext::new(field_values.clone(), secrets.clone());

    // 4b. Resolve content-based file mappings (e.g. credentials from secrets)
    for file_mapping in &runtime_def.files {
        if let Some(ref content_template) = file_mapping.content {
            if let Ok(content) = ctx.interpolate(content_template) {
                let to = ctx.interpolate(&file_mapping.to)
                    .unwrap_or_else(|_| file_mapping.to.clone());
                resolved_files.push(ResolvedFile {
                    content,
                    to,
                    mode: file_mapping.mode.clone(),
                });
            }
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

    // Append optional args where the field has a value
    for opt in &runtime_def.command.optional {
        if ctx.has_field(&opt.when) {
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

fn toml_value_to_string(v: &toml::Value) -> String {
    match v {
        toml::Value::String(s) => s.clone(),
        toml::Value::Integer(i) => i.to_string(),
        toml::Value::Float(f) => f.to_string(),
        toml::Value::Boolean(b) => b.to_string(),
        other => other.to_string(),
    }
}

/// Find a file by name in the search path under a subdirectory.
/// Tries with and without common extensions (.md, .toml, .txt).
fn find_and_read_file(search_path: &[PathBuf], subdir: &str, name: &str) -> Option<String> {
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
    fn field_type_serde() {
        let ft = FieldType::File;
        let json = serde_json::to_string(&ft).unwrap();
        assert_eq!(json, "\"file\"");
    }

    #[test]
    fn metric_source_default() {
        assert_eq!(default_metric_source(), MetricSource::Runtime);
    }
}
