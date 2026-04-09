use anyhow::{Context, Result};
use ox_core::events::*;
use std::path::Path;

/// Derived cx event to be appended to the event log after a merge.
pub struct DerivedCxEvent {
    pub event_type: EventType,
    pub data: serde_json::Value,
}

/// Result of polling cx log.
pub struct CxPollResult {
    pub events: Vec<DerivedCxEvent>,
    /// The hash of the most recent commit seen, to use as --since next time.
    pub latest_hash: Option<String>,
}

/// Run `cx log --json --since <sha>` in the repo and derive ox events.
/// If `since_sha` is None (first boot), snapshot current ready nodes instead
/// of replaying the full history.
pub fn poll_cx_log(repo_path: &Path, since_sha: Option<&str>) -> Result<CxPollResult> {
    let since = match since_sha {
        Some(sha) => sha,
        None => {
            // First boot: snapshot current state instead of replaying history
            return snapshot_current_ready_nodes(repo_path);
        }
    };

    let output = std::process::Command::new("cx")
        .args(["log", "--json", "--since", since])
        .current_dir(repo_path)
        .output()
        .context("running cx log")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("cx log failed: {stderr}");
    }

    let entries: Vec<CxLogEntry> =
        serde_json::from_slice(&output.stdout).context("parsing cx log output")?;

    if entries.is_empty() {
        return Ok(CxPollResult {
            events: vec![],
            latest_hash: None,
        });
    }

    // cx log returns newest-first; the first entry has the latest hash
    let latest_hash = Some(entries[0].hash.clone());

    let mut events = vec![];
    for entry in &entries {
        for change in &entry.changes {
            if let Some(ev) = derive_event(repo_path, change) {
                events.push(ev);
            }
        }
    }

    Ok(CxPollResult {
        events,
        latest_hash,
    })
}

/// Convenience wrapper used after merges — derives cx events between two SHAs.
pub fn derive_cx_events_for_merge(
    repo_path: &Path,
    prev_sha: &str,
) -> Result<Vec<DerivedCxEvent>> {
    let result = poll_cx_log(repo_path, Some(prev_sha))?;
    Ok(result.events)
}

fn derive_event(repo_path: &Path, change: &serde_json::Value) -> Option<DerivedCxEvent> {
    let action = change["action"].as_str()?;
    let node_id = change["node_id"].as_str()?;

    match action {
        "created" | "modified" => {
            // For created: state is directly on the change
            // For modified: state transition is in fields.state.to
            let new_state = if action == "created" {
                change["state"].as_str().map(String::from)
            } else {
                change["fields"]["state"]["to"]
                    .as_str()
                    .map(String::from)
            };

            let new_state = new_state.as_deref()?;

            let tags = extract_tags(repo_path, node_id, change);

            match new_state {
                "ready" => Some(DerivedCxEvent {
                    event_type: EventType::CxTaskReady,
                    data: serde_json::to_value(CxTaskReadyData {
                        node_id: node_id.to_string(),
                        tags,
                        workflow: None,
                    })
                    .unwrap(),
                }),
                "claimed" => Some(DerivedCxEvent {
                    event_type: EventType::CxTaskClaimed,
                    data: serde_json::to_value(CxTaskClaimedData {
                        node_id: node_id.to_string(),
                        part: None,
                    })
                    .unwrap(),
                }),
                "integrated" => Some(DerivedCxEvent {
                    event_type: EventType::CxTaskIntegrated,
                    data: serde_json::to_value(CxTaskIntegratedData {
                        node_id: node_id.to_string(),
                    })
                    .unwrap(),
                }),
                _ => None,
            }
        }
        "comment_added" => Some(DerivedCxEvent {
            event_type: EventType::CxCommentAdded,
            data: serde_json::to_value(CxCommentAddedData {
                node_id: node_id.to_string(),
                tag: change["tag"].as_str().map(String::from),
                author: change["author"].as_str().map(String::from),
            })
            .unwrap(),
        }),
        _ => None,
    }
}

/// Extract tags from a cx log change entry.
/// cx log includes tags on both "created" and "modified" entries.
fn extract_tags(_repo_path: &Path, _node_id: &str, change: &serde_json::Value) -> Vec<String> {
    // Tags directly on the change (created or modified with state change)
    if let Some(tags) = change["tags"].as_array() {
        return tags
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect();
    }
    // Modified — tag changes in fields.tags.to
    if let Some(tags_to) = change["fields"]["tags"]["to"].as_array() {
        return tags_to
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect();
    }
    vec![]
}

/// On first boot, snapshot the current cx state instead of replaying history.
/// Emits cx.task_ready only for nodes that are currently ready.
fn snapshot_current_ready_nodes(repo_path: &Path) -> Result<CxPollResult> {
    // Get current HEAD as the cursor
    let head_output = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo_path)
        .output()
        .context("git rev-parse HEAD")?;
    let head = String::from_utf8_lossy(&head_output.stdout).trim().to_string();

    // List all nodes with their current state
    let output = std::process::Command::new("cx")
        .args(["list", "--json"])
        .current_dir(repo_path)
        .output()
        .context("running cx list --json")?;

    if !output.status.success() {
        // cx might not be initialized — that's fine, no events
        return Ok(CxPollResult {
            events: vec![],
            latest_hash: Some(head),
        });
    }

    let nodes: Vec<serde_json::Value> =
        serde_json::from_slice(&output.stdout).unwrap_or_default();

    let mut events = vec![];
    for node in &nodes {
        let state = node["state"].as_str().unwrap_or("");
        let node_id = node["id"].as_str().unwrap_or("");
        if state != "ready" || node_id.is_empty() {
            continue;
        }
        let tags: Vec<String> = node["tags"]
            .as_array()
            .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default();

        events.push(DerivedCxEvent {
            event_type: EventType::CxTaskReady,
            data: serde_json::to_value(CxTaskReadyData {
                node_id: node_id.to_string(),
                tags,
                workflow: None,
            })
            .unwrap(),
        });
    }

    tracing::info!(ready_count = events.len(), "cx snapshot: initial ready nodes");

    Ok(CxPollResult {
        events,
        latest_hash: Some(head),
    })
}

#[derive(serde::Deserialize)]
struct CxLogEntry {
    changes: Vec<serde_json::Value>,
    hash: String,
    #[allow(dead_code)]
    date: String,
    #[allow(dead_code)]
    subject: String,
}
