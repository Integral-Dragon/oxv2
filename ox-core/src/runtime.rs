use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

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

/// A file to place in the workspace, with content inline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedFile {
    /// The file content.
    pub content: String,
    /// Destination path relative to workspace root.
    pub to: String,
    /// POSIX permission string.
    #[serde(default = "default_file_mode")]
    pub mode: String,
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
