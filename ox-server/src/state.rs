use anyhow::Result;
use arc_swap::ArcSwap;
use ox_core::config::{self, OxConfig};
use ox_core::persona::PersonaDef;
use ox_core::runtime::RuntimeDef;
use ox_core::workflow::{TriggerDef, WorkflowDef, WorkflowEngine};
use rusqlite::Connection;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::events::EventBus;

/// Durable server state — lives for the process lifetime, never reloaded.
pub struct ServerState {
    pub bus: EventBus,
    pub repo_path: PathBuf,
    pub pty_relays: crate::pty_relay::PtyRelays,
    /// Hot-reloadable configuration. Swapped atomically on SIGHUP or API reload.
    pub hot: ArcSwap<HotConfig>,
}

/// Hot-reloadable configuration — rebuilt from disk on reload.
pub struct HotConfig {
    pub config: OxConfig,
    pub workflows: HashMap<String, WorkflowEngine>,
    pub triggers: Vec<TriggerDef>,
    pub runtimes: HashMap<String, RuntimeDef>,
    pub personas: HashMap<String, PersonaDef>,
    pub search_path: Vec<PathBuf>,
}

impl HotConfig {
    /// Load all hot configuration from disk.
    pub fn load(repo_root: &Path) -> Result<Self> {
        let search_path = config::resolve_search_path(repo_root);

        // Load ox config (merges config.toml across search path)
        let config = config::load_config(&search_path);

        // Load trigger definitions from files listed in config
        let triggers = config::load_triggers(&config);

        // Load workflow definitions
        let mut workflows = HashMap::new();
        for (name, path) in config::load_all_configs(&search_path, "workflows") {
            match WorkflowDef::from_file(&path) {
                Ok(def) => {
                    tracing::info!(workflow = %name, path = %path.display(), "loaded workflow");
                    workflows.insert(def.name.clone(), WorkflowEngine::from_def(def));
                }
                Err(e) => {
                    tracing::warn!(workflow = %name, path = %path.display(), err = %e, "failed to load workflow");
                }
            }
        }

        // Load runtime definitions
        let mut runtimes = HashMap::new();
        for (name, path) in config::load_all_configs(&search_path, "runtimes") {
            match RuntimeDef::from_file(&path) {
                Ok(def) => {
                    tracing::info!(runtime = %name, path = %path.display(), "loaded runtime");
                    runtimes.insert(def.name.clone(), def);
                }
                Err(e) => {
                    tracing::warn!(runtime = %name, path = %path.display(), err = %e, "failed to load runtime");
                }
            }
        }

        // Load persona definitions (markdown with YAML frontmatter)
        let personas = ox_core::persona::load_personas(&search_path);
        tracing::info!(count = personas.len(), "loaded personas");

        // Validate persona vars against runtime definitions
        let persona_errors = ox_core::persona::validate_personas(&personas, &runtimes);
        for err in &persona_errors {
            tracing::error!("{err}");
        }
        if !persona_errors.is_empty() {
            anyhow::bail!(
                "{} persona validation error(s) — see log above",
                persona_errors.len()
            );
        }

        Ok(Self {
            config,
            workflows,
            triggers,
            runtimes,
            personas,
            search_path,
        })
    }
}

impl ServerState {
    pub fn new(conn: Connection, repo_root: &str) -> Result<Self> {
        let bus = EventBus::new(conn)?;
        let repo_path = PathBuf::from(repo_root);
        let hot = HotConfig::load(&repo_path)?;

        Ok(Self {
            bus,
            repo_path,
            pty_relays: crate::pty_relay::new_relays(),
            hot: ArcSwap::new(Arc::new(hot)),
        })
    }
}
