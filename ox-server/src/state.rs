use anyhow::Result;
use ox_core::config::{self, OxConfig};
use ox_core::runtime::RuntimeDef;
use ox_core::workflow::{TriggerDef, WorkflowDef, WorkflowEngine};
use rusqlite::Connection;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::events::EventBus;

/// The shared server state. Held behind an Arc in the Axum state.
pub struct ServerState {
    pub bus: EventBus,
    pub config: OxConfig,
    pub workflows: HashMap<String, WorkflowEngine>,
    pub triggers: Vec<TriggerDef>,
    pub runtimes: HashMap<String, RuntimeDef>,
    pub search_path: Vec<PathBuf>,
    pub repo_path: PathBuf,
    pub pty_relays: crate::pty_relay::PtyRelays,
}

impl ServerState {
    pub fn new(conn: Connection, repo_root: &str) -> Result<Self> {
        let bus = EventBus::new(conn)?;

        let search_path = config::resolve_search_path(Path::new(repo_root));

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

        Ok(Self {
            bus,
            config,
            workflows,
            triggers,
            runtimes,
            search_path,
            repo_path: PathBuf::from(repo_root),
            pty_relays: crate::pty_relay::new_relays(),
        })
    }
}
