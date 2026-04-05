use anyhow::Result;
use ox_core::config;
use ox_core::workflow::{WorkflowDef, WorkflowEngine};
use rusqlite::Connection;
use std::collections::HashMap;
use std::path::Path;

use crate::events::EventBus;

/// The shared server state. Held behind an Arc in the Axum state.
pub struct ServerState {
    pub bus: EventBus,
    pub workflows: HashMap<String, WorkflowEngine>,
}

impl ServerState {
    pub fn new(conn: Connection, repo_root: &str) -> Result<Self> {
        let bus = EventBus::new(conn)?;

        // Load workflow definitions from the config search path
        let search_path = config::resolve_search_path(Path::new(repo_root));
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

        Ok(Self { bus, workflows })
    }
}
