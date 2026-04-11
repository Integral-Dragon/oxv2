use std::collections::HashMap;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum InterpolationError {
    #[error("missing field: {0}")]
    MissingField(String),
    #[error("missing secret: {0}")]
    MissingSecret(String),
}

/// Context for resolving `{name}` field references and `{secret.name}` secret references.
pub struct InterpolationContext {
    values: HashMap<String, String>,
    secrets: HashMap<String, String>,
}

impl InterpolationContext {
    pub fn new(values: HashMap<String, String>, secrets: HashMap<String, String>) -> Self {
        Self { values, secrets }
    }

    /// Create a context with only field values (no secrets).
    pub fn fields_only(values: HashMap<String, String>) -> Self {
        Self {
            values,
            secrets: HashMap::new(),
        }
    }

    /// Interpolate a template string. Resolves `{name}` from values and `{secret.name}` from secrets.
    pub fn interpolate(&self, template: &str) -> Result<String, InterpolationError> {
        let mut result = String::with_capacity(template.len());
        let mut chars = template.chars().peekable();

        while let Some(ch) = chars.next() {
            if ch == '{' {
                let name: String = chars.by_ref().take_while(|&c| c != '}').collect();
                if let Some(secret_name) = name.strip_prefix("secret.") {
                    match self.secrets.get(secret_name) {
                        Some(value) => result.push_str(value),
                        None => {
                            return Err(InterpolationError::MissingSecret(
                                secret_name.to_string(),
                            ))
                        }
                    }
                } else {
                    match self.values.get(&name) {
                        Some(value) => result.push_str(value),
                        None => return Err(InterpolationError::MissingField(name)),
                    }
                }
            } else {
                result.push(ch);
            }
        }

        Ok(result)
    }

    /// Check if a field has a value (for `optional` blocks).
    pub fn has_field(&self, name: &str) -> bool {
        self.values.contains_key(name)
    }

    /// Collect all `{secret.NAME}` references from a template without resolving them.
    pub fn collect_secret_refs(template: &str) -> Vec<String> {
        let mut refs = vec![];
        let mut chars = template.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch == '{' {
                let name: String = chars.by_ref().take_while(|&c| c != '}').collect();
                if let Some(secret_name) = name.strip_prefix("secret.") {
                    refs.push(secret_name.to_string());
                }
            }
        }
        refs
    }

    /// Collect all secret refs from multiple templates (deduped).
    pub fn collect_all_secret_refs<'a>(templates: impl IntoIterator<Item = &'a str>) -> Vec<String> {
        let mut seen = std::collections::HashSet::new();
        let mut refs = vec![];
        for t in templates {
            for r in Self::collect_secret_refs(t) {
                if seen.insert(r.clone()) {
                    refs.push(r);
                }
            }
        }
        refs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_interpolation() {
        let mut values = HashMap::new();
        values.insert("name".into(), "world".into());
        let ctx = InterpolationContext::fields_only(values);
        assert_eq!(ctx.interpolate("hello {name}").unwrap(), "hello world");
    }

    #[test]
    fn secret_interpolation() {
        let mut secrets = HashMap::new();
        secrets.insert("api_key".into(), "sk-123".into());
        let ctx = InterpolationContext::new(HashMap::new(), secrets);
        assert_eq!(
            ctx.interpolate("{secret.api_key}").unwrap(),
            "sk-123"
        );
    }

    #[test]
    fn mixed_interpolation() {
        let mut values = HashMap::new();
        values.insert("model".into(), "sonnet".into());
        let mut secrets = HashMap::new();
        secrets.insert("key".into(), "secret-val".into());
        let ctx = InterpolationContext::new(values, secrets);
        assert_eq!(
            ctx.interpolate("--model {model} --key {secret.key}").unwrap(),
            "--model sonnet --key secret-val"
        );
    }

    #[test]
    fn missing_field_error() {
        let ctx = InterpolationContext::fields_only(HashMap::new());
        let err = ctx.interpolate("{missing}").unwrap_err();
        assert!(matches!(err, InterpolationError::MissingField(ref s) if s == "missing"));
    }

    #[test]
    fn missing_secret_error() {
        let ctx = InterpolationContext::fields_only(HashMap::new());
        let err = ctx.interpolate("{secret.missing}").unwrap_err();
        assert!(matches!(err, InterpolationError::MissingSecret(ref s) if s == "missing"));
    }

    #[test]
    fn no_placeholders() {
        let ctx = InterpolationContext::fields_only(HashMap::new());
        assert_eq!(ctx.interpolate("plain text").unwrap(), "plain text");
    }

    #[test]
    fn collect_secret_refs_basic() {
        let refs =
            InterpolationContext::collect_secret_refs("key={secret.api_key} tok={secret.token}");
        assert_eq!(refs, vec!["api_key", "token"]);
    }

    #[test]
    fn collect_secret_refs_no_secrets() {
        let refs = InterpolationContext::collect_secret_refs("hello {name}");
        assert!(refs.is_empty());
    }

    #[test]
    fn collect_all_dedupes() {
        let refs = InterpolationContext::collect_all_secret_refs([
            "{secret.a} {secret.b}",
            "{secret.b} {secret.c}",
        ]);
        assert_eq!(refs, vec!["a", "b", "c"]);
    }

    #[test]
    fn has_field() {
        let mut values = HashMap::new();
        values.insert("model".into(), "sonnet".into());
        let ctx = InterpolationContext::fields_only(values);
        assert!(ctx.has_field("model"));
        assert!(!ctx.has_field("persona"));
    }
}
