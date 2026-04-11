use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::runtime::RuntimeDef;

/// A persona definition, loaded from a markdown file with YAML frontmatter.
///
/// The frontmatter declares structural fields (runtime, skills, secrets)
/// plus arbitrary vars that flow through to the runtime as defaults.
/// The markdown body is the agent's instructions.
#[derive(Debug, Clone)]
pub struct PersonaDef {
    /// Persona name, derived from file path (e.g. "inspired/software-engineer").
    pub name: String,
    /// Runtime to use (e.g. "claude"). If None, must be specified at the step level.
    pub runtime: Option<String>,
    /// Skills this persona needs.
    pub skills: Vec<String>,
    /// Secrets this persona expects (informational).
    pub secrets: Vec<String>,
    /// Runtime variable defaults from frontmatter (e.g. model, temperature,
    /// max_tokens — anything the runtime definition declares as a var).
    /// These are injected into the runtime spec as defaults, overridable
    /// at the step level.
    pub vars: HashMap<String, String>,
    /// The markdown body — everything after the frontmatter.
    pub instructions: String,
}

/// Raw YAML frontmatter fields.
///
/// Structural fields are parsed explicitly. Everything else is captured
/// by the flattened `vars` map and passed through as runtime var defaults.
#[derive(Debug, Default, Deserialize)]
struct PersonaFrontmatter {
    #[serde(default)]
    runtime: Option<String>,
    #[serde(default)]
    skills: Vec<String>,
    #[serde(default)]
    secrets: Vec<String>,
    /// All other frontmatter fields become runtime var defaults.
    #[serde(flatten)]
    vars: HashMap<String, serde_yaml::Value>,
}

/// Parse a persona from its file content (markdown with optional YAML frontmatter).
///
/// Structural fields (`runtime`, `skills`, `secrets`) are parsed explicitly.
/// All other frontmatter fields are captured as runtime var defaults.
///
/// ```markdown
/// ---
/// runtime: claude
/// model: sonnet
/// temperature: 0.7
/// skills: [shell, web-search]
/// ---
///
/// You are a software engineer...
/// ```
///
/// Files without frontmatter are treated as instructions-only (backwards compatible
/// with plain .md persona files).
pub fn parse_persona(name: &str, content: &str) -> PersonaDef {
    let (frontmatter, instructions) = split_frontmatter(content);

    let fm: PersonaFrontmatter = frontmatter
        .and_then(|yaml| serde_yaml::from_str(yaml).ok())
        .unwrap_or_default();

    // Convert YAML values to strings for runtime var injection.
    let vars: HashMap<String, String> = fm.vars.into_iter()
        .map(|(k, v)| (k, yaml_value_to_string(&v)))
        .collect();

    PersonaDef {
        name: name.to_string(),
        runtime: fm.runtime,
        skills: fm.skills,
        secrets: fm.secrets,
        vars,
        instructions,
    }
}

/// Convert a serde_yaml::Value to a string for use as a runtime var.
fn yaml_value_to_string(v: &serde_yaml::Value) -> String {
    match v {
        serde_yaml::Value::String(s) => s.clone(),
        serde_yaml::Value::Bool(b) => b.to_string(),
        serde_yaml::Value::Number(n) => n.to_string(),
        other => format!("{other:?}"),
    }
}

/// Split markdown content into optional YAML frontmatter and body.
///
/// Frontmatter is delimited by `---` on its own line at the start of the file.
/// Returns (Some(yaml_str), body) if frontmatter found, (None, full_content) otherwise.
fn split_frontmatter(content: &str) -> (Option<&str>, String) {
    let trimmed = content.trim_start();

    if !trimmed.starts_with("---") {
        return (None, content.to_string());
    }

    // Find the closing ---
    let after_open = &trimmed[3..];
    let after_open = after_open.strip_prefix('\n').unwrap_or(after_open);

    // Handle both `---\n---` (empty frontmatter) and `---\ncontent\n---`
    let close = if after_open.starts_with("---") {
        Some((0, 3))
    } else {
        after_open.find("\n---").map(|pos| (pos, pos + 4))
    };

    if let Some((yaml_end, body_offset)) = close {
        let yaml = &after_open[..yaml_end];
        let body_start = body_offset;
        let body = if body_start < after_open.len() {
            after_open[body_start..].trim_start_matches('\n')
        } else {
            ""
        };
        (Some(yaml.trim()), body.to_string())
    } else {
        // Opening --- but no closing --- : treat whole thing as content
        (None, content.to_string())
    }
}

/// Load a single persona from a file path.
/// The name is derived from the relative path (e.g. "inspired/software-engineer").
pub fn load_persona(path: &Path, name: &str) -> Result<PersonaDef> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("reading persona file: {}", path.display()))?;
    Ok(parse_persona(name, &content))
}

/// Load all personas from the search path.
///
/// Walks `personas/` subdirectory in each search-path entry. Persona names are
/// derived from relative paths: `personas/inspired/software-engineer.md`
/// becomes `"inspired/software-engineer"`.
///
/// First match per name wins (higher-priority directories shadow lower ones).
pub fn load_personas(search_path: &[PathBuf]) -> HashMap<String, PersonaDef> {
    let mut personas = HashMap::new();

    for dir in search_path {
        let personas_dir = dir.join("personas");
        if !personas_dir.is_dir() {
            continue;
        }
        walk_personas_dir(&personas_dir, &personas_dir, &mut personas);
    }

    personas
}

/// Validate all personas against the loaded runtime definitions.
///
/// Errors if a persona sets a var that its runtime doesn't declare
/// (catches typos like `modle` instead of `model`). Personas that
/// don't specify a runtime are skipped.
pub fn validate_personas(
    personas: &HashMap<String, PersonaDef>,
    runtimes: &HashMap<String, RuntimeDef>,
) -> Vec<String> {
    let mut errors = Vec::new();

    for (name, persona) in personas {
        let Some(rt_name) = &persona.runtime else {
            continue;
        };
        let Some(runtime_def) = runtimes.get(rt_name) else {
            errors.push(format!(
                "persona '{name}': references unknown runtime '{rt_name}'"
            ));
            continue;
        };

        for var_name in persona.vars.keys() {
            if !runtime_def.vars.contains_key(var_name) {
                errors.push(format!(
                    "persona '{name}': sets var '{var_name}' which runtime '{rt_name}' does not declare"
                ));
            }
        }
    }

    errors
}

/// Recursively walk a personas directory, loading .md files.
fn walk_personas_dir(
    base: &Path,
    current: &Path,
    personas: &mut HashMap<String, PersonaDef>,
) {
    let entries = match std::fs::read_dir(current) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_personas_dir(base, &path, personas);
        } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
            // Derive name from relative path, stripping .md extension
            let rel = path.strip_prefix(base).unwrap_or(&path);
            let name = rel
                .with_extension("")
                .to_string_lossy()
                .replace('\\', "/"); // normalize Windows paths

            if personas.contains_key(&name) {
                continue; // first match wins
            }

            match load_persona(&path, &name) {
                Ok(persona) => {
                    tracing::info!(name = %name, "loaded persona");
                    personas.insert(name, persona);
                }
                Err(e) => {
                    tracing::warn!(path = %path.display(), err = %e, "failed to load persona");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_with_full_frontmatter() {
        let content = r#"---
runtime: claude
model: sonnet
skills: [shell, web-search]
secrets: [api_key]
---

You are a software engineer."#;

        let persona = parse_persona("test/engineer", content);
        assert_eq!(persona.name, "test/engineer");
        assert_eq!(persona.runtime.as_deref(), Some("claude"));
        assert_eq!(persona.vars.get("model").map(|s| s.as_str()), Some("sonnet"));
        assert_eq!(persona.skills, vec!["shell", "web-search"]);
        assert_eq!(persona.secrets, vec!["api_key"]);
        assert_eq!(persona.instructions, "You are a software engineer.");
    }

    #[test]
    fn parse_no_frontmatter() {
        let content = "You are a plain persona with no metadata.";
        let persona = parse_persona("plain", content);
        assert_eq!(persona.name, "plain");
        assert!(persona.runtime.is_none());
        assert!(persona.vars.is_empty());
        assert!(persona.skills.is_empty());
        assert_eq!(persona.instructions, content);
    }

    #[test]
    fn parse_empty_frontmatter() {
        let content = "---\n---\nJust instructions.";
        let persona = parse_persona("empty-fm", content);
        assert!(persona.runtime.is_none());
        assert!(persona.vars.is_empty());
        assert_eq!(persona.instructions, "Just instructions.");
    }

    #[test]
    fn parse_partial_frontmatter() {
        let content = "---\nruntime: codex\n---\n\nYou use codex.";
        let persona = parse_persona("partial", content);
        assert_eq!(persona.runtime.as_deref(), Some("codex"));
        assert!(persona.vars.is_empty());
        assert!(persona.skills.is_empty());
        assert_eq!(persona.instructions, "You use codex.");
    }

    #[test]
    fn parse_arbitrary_vars() {
        let content = "---\nruntime: claude\nmodel: opus\ntemperature: 0.7\nmax_tokens: 4096\n---\n\nYou are creative.";
        let persona = parse_persona("custom", content);
        assert_eq!(persona.runtime.as_deref(), Some("claude"));
        assert_eq!(persona.vars.get("model").map(|s| s.as_str()), Some("opus"));
        assert_eq!(persona.vars.get("temperature").map(|s| s.as_str()), Some("0.7"));
        assert_eq!(persona.vars.get("max_tokens").map(|s| s.as_str()), Some("4096"));
    }

    #[test]
    fn parse_frontmatter_preserves_body_formatting() {
        let content = "---\nruntime: claude\n---\n\n# Heading\n\n- bullet one\n- bullet two\n";
        let persona = parse_persona("formatted", content);
        assert!(persona.instructions.contains("# Heading"));
        assert!(persona.instructions.contains("- bullet one"));
    }

    #[test]
    fn parse_no_closing_delimiter_treated_as_plain() {
        let content = "---\nThis looks like frontmatter but has no closing delimiter.";
        let persona = parse_persona("broken", content);
        assert!(persona.runtime.is_none());
        assert_eq!(persona.instructions, content);
    }

    #[test]
    fn load_personas_from_search_path() {
        let tmp = std::env::temp_dir().join("ox-persona-test");
        let personas_dir = tmp.join("personas/team");
        std::fs::create_dir_all(&personas_dir).unwrap();

        std::fs::write(
            personas_dir.join("reviewer.md"),
            "---\nruntime: claude\nmodel: opus\n---\n\nYou review code.",
        ).unwrap();

        let result = load_personas(&[tmp.clone()]);
        assert!(result.contains_key("team/reviewer"));
        let p = &result["team/reviewer"];
        assert_eq!(p.runtime.as_deref(), Some("claude"));
        assert_eq!(p.vars.get("model").map(|s| s.as_str()), Some("opus"));
        assert_eq!(p.instructions, "You review code.");

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn validate_catches_unknown_var() {
        use indexmap::IndexMap;
        use crate::workflow::VarDef;

        let mut personas = HashMap::new();
        personas.insert("test/eng".to_string(), PersonaDef {
            name: "test/eng".into(),
            runtime: Some("claude".into()),
            skills: vec![],
            secrets: vec![],
            vars: HashMap::from([
                ("model".into(), "sonnet".into()),
                ("modle".into(), "typo".into()),  // typo
            ]),
            instructions: String::new(),
        });

        let mut runtimes = HashMap::new();
        let mut vars = IndexMap::new();
        vars.insert("model".into(), VarDef {
            var_type: Default::default(),
            required: false,
            default: None,
            description: None,
            search_dir: None,
        });
        runtimes.insert("claude".into(), RuntimeDef {
            name: "claude".into(),
            vars,
            command: crate::runtime::CommandDef {
                cmd: vec!["claude".into()],
                interactive_cmd: None,
                optional: vec![],
            },
            files: vec![],
            env: HashMap::new(),
            proxy: vec![],
            metrics: vec![],
        });

        let errors = validate_personas(&personas, &runtimes);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("modle"));
        assert!(errors[0].contains("does not declare"));
    }

    #[test]
    fn validate_catches_unknown_runtime() {
        let mut personas = HashMap::new();
        personas.insert("test/eng".to_string(), PersonaDef {
            name: "test/eng".into(),
            runtime: Some("nonexistent".into()),
            skills: vec![],
            secrets: vec![],
            vars: HashMap::new(),
            instructions: String::new(),
        });

        let runtimes = HashMap::new();
        let errors = validate_personas(&personas, &runtimes);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("unknown runtime"));
    }

    #[test]
    fn validate_skips_persona_without_runtime() {
        let mut personas = HashMap::new();
        personas.insert("plain".to_string(), PersonaDef {
            name: "plain".into(),
            runtime: None,
            skills: vec![],
            secrets: vec![],
            vars: HashMap::from([("anything".into(), "value".into())]),
            instructions: String::new(),
        });

        let runtimes = HashMap::new();
        let errors = validate_personas(&personas, &runtimes);
        assert!(errors.is_empty());
    }

    #[test]
    fn validate_passes_valid_persona() {
        use indexmap::IndexMap;
        use crate::workflow::VarDef;

        let mut personas = HashMap::new();
        personas.insert("test/eng".to_string(), PersonaDef {
            name: "test/eng".into(),
            runtime: Some("claude".into()),
            skills: vec![],
            secrets: vec![],
            vars: HashMap::from([("model".into(), "sonnet".into())]),
            instructions: String::new(),
        });

        let mut runtimes = HashMap::new();
        let mut vars = IndexMap::new();
        vars.insert("model".into(), VarDef {
            var_type: Default::default(),
            required: false,
            default: None,
            description: None,
            search_dir: None,
        });
        vars.insert("prompt".into(), VarDef {
            var_type: Default::default(),
            required: false,
            default: Some(String::new()),
            description: None,
            search_dir: None,
        });
        runtimes.insert("claude".into(), RuntimeDef {
            name: "claude".into(),
            vars,
            command: crate::runtime::CommandDef {
                cmd: vec!["claude".into()],
                interactive_cmd: None,
                optional: vec![],
            },
            files: vec![],
            env: HashMap::new(),
            proxy: vec![],
            metrics: vec![],
        });

        let errors = validate_personas(&personas, &runtimes);
        assert!(errors.is_empty());
    }
}
