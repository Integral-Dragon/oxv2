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

/// Run `cx log --json [--since <sha>]` in the repo and derive ox events
/// from the structured change entries. If `since_sha` is None, fetches the
/// full log (used on first run to catch up on existing cx state).
pub fn poll_cx_log(repo_path: &Path, since_sha: Option<&str>) -> Result<CxPollResult> {
    let mut args = vec!["log", "--json"];
    if let Some(sha) = since_sha {
        args.push("--since");
        args.push(sha);
    }

    let output = std::process::Command::new("cx")
        .args(&args)
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
            if let Some(ev) = derive_event(change) {
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

fn derive_event(change: &serde_json::Value) -> Option<DerivedCxEvent> {
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

            let tags = extract_tags(change);

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
/// For "created": tags are directly on the entry.
/// For "modified": tags are in fields.tags.to.
fn extract_tags(change: &serde_json::Value) -> Vec<String> {
    // Created — tags directly on the change
    if let Some(tags) = change["tags"].as_array() {
        return tags
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect();
    }
    // Modified — tag changes in fields
    if let Some(tags_to) = change["fields"]["tags"]["to"].as_array() {
        return tags_to
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect();
    }
    vec![]
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
