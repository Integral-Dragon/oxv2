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
    // For tag-only modifications we need the node's *current* state to
    // decide whether the predicate (ready + matching tag) is satisfied.
    // The pure inner function handles every other case without I/O; we
    // do the disk lookup here only when it's needed.
    let needs_current_state = change["action"].as_str() == Some("modified")
        && change["fields"]["state"].is_null()
        && !change["fields"]["tags"].is_null();
    let current_state = if needs_current_state {
        change["node_id"]
            .as_str()
            .and_then(|id| read_current_node_state(repo_path, id))
    } else {
        None
    };
    derive_event_inner(change, current_state.as_deref())
}

/// Pure derivation: given a cx log change entry and (optionally) the
/// node's current state, return the ox event it should emit. Split out
/// for unit testing without disk I/O.
fn derive_event_inner(
    change: &serde_json::Value,
    current_state: Option<&str>,
) -> Option<DerivedCxEvent> {
    let action = change["action"].as_str()?;
    let node_id = change["node_id"].as_str()?;

    match action {
        "created" | "modified" => {
            // For created: state is directly on the change
            // For modified with state transition: fields.state.to
            // For modified with tag-only change: fall back to current_state
            let new_state = if action == "created" {
                change["state"].as_str().map(String::from)
            } else {
                change["fields"]["state"]["to"]
                    .as_str()
                    .map(String::from)
                    .or_else(|| {
                        // Tag-only modification — caller pre-fetched the
                        // current node state. Only re-fire for ready
                        // nodes; tag changes on claimed/integrated nodes
                        // are informational and would otherwise spuriously
                        // re-emit CxTaskClaimed/CxTaskIntegrated events.
                        if !change["fields"]["tags"].is_null()
                            && current_state == Some("ready")
                        {
                            Some("ready".to_string())
                        } else {
                            None
                        }
                    })
            };

            let new_state = new_state.as_deref()?;

            let tags = extract_tags_from_change(change);

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

/// Read a node's current state from `.complex/nodes/<id>.json`. Returns
/// `None` if the file doesn't exist, can't be parsed, or has no state field.
fn read_current_node_state(repo_path: &Path, node_id: &str) -> Option<String> {
    let path = repo_path.join(".complex/nodes").join(format!("{node_id}.json"));
    let bytes = std::fs::read(&path).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    v.get("state").and_then(|s| s.as_str()).map(String::from)
}

/// Extract tags from a cx log change entry.
/// cx log includes tags on both "created" and "modified" entries.
fn extract_tags_from_change(change: &serde_json::Value) -> Vec<String> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn modified_with_state_to_ready_emits_task_ready() {
        let change = json!({
            "action": "modified",
            "node_id": "aBcD",
            "fields": { "state": { "from": "latent", "to": "ready" } },
            "tags": ["workflow:code-task"]
        });
        let ev = derive_event_inner(&change, None).expect("event derived");
        assert!(matches!(ev.event_type, EventType::CxTaskReady));
        let data: CxTaskReadyData = serde_json::from_value(ev.data).unwrap();
        assert_eq!(data.node_id, "aBcD");
        assert_eq!(data.tags, vec!["workflow:code-task"]);
    }

    /// The CcoT scenario: a node that was already in state=ready had a
    /// tag added later. The cx log change has fields.tags but no
    /// fields.state. Previously this returned None and the trigger
    /// silently never fired. Now: if the caller pre-fetched the current
    /// state and it's "ready", emit CxTaskReady so reconciliation
    /// downstream sees the predicate.
    #[test]
    fn modified_tag_only_on_ready_node_emits_task_ready() {
        let change = json!({
            "action": "modified",
            "node_id": "CcoT",
            "fields": { "tags": { "from": null, "to": ["workflow:code-task"] } }
        });
        let ev = derive_event_inner(&change, Some("ready")).expect("event derived");
        assert!(matches!(ev.event_type, EventType::CxTaskReady));
        let data: CxTaskReadyData = serde_json::from_value(ev.data).unwrap();
        assert_eq!(data.node_id, "CcoT");
        assert_eq!(data.tags, vec!["workflow:code-task"]);
    }

    /// Tag-only change on a non-ready node must NOT emit CxTaskReady.
    /// Otherwise we'd spam events for tag updates on latent or claimed
    /// nodes.
    #[test]
    fn modified_tag_only_on_non_ready_node_emits_nothing() {
        let change = json!({
            "action": "modified",
            "node_id": "CcoT",
            "fields": { "tags": { "from": null, "to": ["workflow:code-task"] } }
        });
        assert!(derive_event_inner(&change, Some("latent")).is_none());
        assert!(derive_event_inner(&change, Some("claimed")).is_none());
        assert!(derive_event_inner(&change, Some("integrated")).is_none());
    }

    /// Tag-only change with no current state available (e.g. node file
    /// missing) must not emit anything — we can't make a decision.
    #[test]
    fn modified_tag_only_without_current_state_emits_nothing() {
        let change = json!({
            "action": "modified",
            "node_id": "CcoT",
            "fields": { "tags": { "from": null, "to": ["workflow:code-task"] } }
        });
        assert!(derive_event_inner(&change, None).is_none());
    }

    /// State transitions still take precedence over current_state — a
    /// transition to ready always wins regardless of what current_state
    /// happens to be (it might be stale anyway since the file may have
    /// been written before the change took effect).
    #[test]
    fn state_transition_takes_precedence_over_current_state() {
        let change = json!({
            "action": "modified",
            "node_id": "aBcD",
            "fields": { "state": { "from": "latent", "to": "claimed" } }
        });
        let ev = derive_event_inner(&change, Some("ready")).expect("event derived");
        assert!(matches!(ev.event_type, EventType::CxTaskClaimed));
    }
}
