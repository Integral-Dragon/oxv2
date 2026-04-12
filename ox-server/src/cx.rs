use anyhow::{Context, Result};
use ox_core::client::{CxNodeSnapshot, CxStateSnapshot};
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

// ── cx as source of truth ──────────────────────────────────────────
//
// Reconciliation reads node state from cx commands rather than from
// the event-derived projection. The projection diverged from disk
// truth whenever events were missed (e.g. an integrate that happened
// outside the cx_log poll window). cx is authoritative; we always
// query it for the current shape.
//
// All cx command shells live here. No other file should invoke
// `Command::new("cx")`.

/// Pure parser for `cx list --json` output. Maps the cx node shape
/// onto the [`CxStateSnapshot`] format the herder consumes via
/// `/api/state/cx`. Uses local `tags` (not `effective_tags`) to
/// match current trigger-evaluation semantics.
pub fn parse_cx_list(stdout: &[u8]) -> Result<CxStateSnapshot> {
    let nodes_json: Vec<serde_json::Value> =
        serde_json::from_slice(stdout).context("parsing cx list output")?;

    let mut snap = CxStateSnapshot::default();
    for node in nodes_json {
        let Some(id) = node.get("id").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(state) = node.get("state").and_then(|v| v.as_str()) else {
            continue;
        };
        let tags: Vec<String> = node
            .get("tags")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default();
        let shadowed = node.get("shadowed").and_then(|v| v.as_bool()).unwrap_or(false);

        snap.nodes.insert(
            id.to_string(),
            CxNodeSnapshot {
                node_id: id.to_string(),
                state: state.to_string(),
                tags,
                shadowed,
                shadow_reason: None,
                comment_count: 0,
            },
        );
    }
    Ok(snap)
}

/// Pure parser for `cx show <id> --json` output. Extracts only the
/// fields downstream consumers care about (state, tags, shadowed).
pub fn parse_cx_show(stdout: &[u8]) -> Option<CxNodeSnapshot> {
    let v: serde_json::Value = serde_json::from_slice(stdout).ok()?;
    let id = v.get("id").and_then(|s| s.as_str())?;
    let state = v.get("state").and_then(|s| s.as_str())?;
    let tags: Vec<String> = v
        .get("tags")
        .and_then(|t| t.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let shadowed = v.get("shadowed").and_then(|b| b.as_bool()).unwrap_or(false);
    Some(CxNodeSnapshot {
        node_id: id.to_string(),
        state: state.to_string(),
        tags,
        shadowed,
        shadow_reason: None,
        comment_count: 0,
    })
}

/// Run `cx list --json` in the given repo and parse it. This is the
/// source-of-truth fetch for trigger reconciliation.
pub fn fetch_cx_state(repo_path: &Path) -> Result<CxStateSnapshot> {
    let out = std::process::Command::new("cx")
        .args(["list", "--json"])
        .current_dir(repo_path)
        .output()
        .context("running cx list")?;
    if !out.status.success() {
        anyhow::bail!("cx list failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    parse_cx_list(&out.stdout)
}

/// Run `cx show <id> --json` for a single node. Returns None if the
/// node does not exist or cx exits non-zero.
pub fn fetch_node(repo_path: &Path, node_id: &str) -> Option<CxNodeSnapshot> {
    let out = std::process::Command::new("cx")
        .args(["show", node_id, "--json"])
        .current_dir(repo_path)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_cx_show(&out.stdout)
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

    // ── parse_cx_list / parse_cx_show ──────────────────────────────
    //
    // These cover the source-of-truth path: cx commands return live
    // state, parsers map them onto the snapshot shape the herder
    // consumes via /api/state/cx. The bug being fixed is that the
    // event-derived projection diverged from cx truth — Ygdt was
    // reported as "ready" by the projection long after cx had it
    // marked "integrated", causing reconcile to fire spurious
    // executions.

    #[test]
    fn parse_cx_list_maps_state_and_tags_correctly() {
        // Mirrors actual `cx list --json` output shape (verified
        // against ccstat). Includes one ready+tagged node, one
        // integrated+tagged node (the Ygdt scenario), one ready
        // untagged node, and one shadowed node — exercises every
        // field reconcile cares about.
        let stdout = br#"[
            {
                "id": "CcoT",
                "state": "ready",
                "tags": ["workflow:code-task"],
                "effective_tags": ["workflow:code-task"],
                "shadowed": false,
                "title": "ready and tagged"
            },
            {
                "id": "Ygdt",
                "state": "integrated",
                "tags": ["workflow:code-task"],
                "effective_tags": ["workflow:code-task"],
                "shadowed": false,
                "title": "integrated but still tagged"
            },
            {
                "id": "fsih",
                "state": "ready",
                "tags": [],
                "effective_tags": [],
                "shadowed": false,
                "title": "ready no tag"
            },
            {
                "id": "stuk",
                "state": "claimed",
                "tags": ["workflow:code-task"],
                "effective_tags": ["workflow:code-task"],
                "shadowed": true,
                "title": "claimed and shadowed"
            }
        ]"#;

        let snap = parse_cx_list(stdout).expect("parses");
        assert_eq!(snap.nodes.len(), 4);

        let ccot = &snap.nodes["CcoT"];
        assert_eq!(ccot.state, "ready");
        assert_eq!(ccot.tags, vec!["workflow:code-task"]);
        assert!(!ccot.shadowed);

        let ygdt = &snap.nodes["Ygdt"];
        assert_eq!(
            ygdt.state, "integrated",
            "Ygdt must report as integrated — this is the bug"
        );
        assert_eq!(ygdt.tags, vec!["workflow:code-task"]);

        let fsih = &snap.nodes["fsih"];
        assert_eq!(fsih.state, "ready");
        assert!(fsih.tags.is_empty());

        let stuk = &snap.nodes["stuk"];
        assert_eq!(stuk.state, "claimed");
        assert!(stuk.shadowed);
    }

    /// `tags` is the local tag list. `effective_tags` includes
    /// inherited ones. Triggers match against `tags`. Pin this so a
    /// later "let's just use effective_tags" change has to be deliberate.
    #[test]
    fn parse_cx_list_uses_local_tags_not_effective() {
        let stdout = br#"[{
            "id": "child",
            "state": "ready",
            "tags": [],
            "effective_tags": ["workflow:from-parent"],
            "shadowed": false,
            "title": "inherits tag"
        }]"#;
        let snap = parse_cx_list(stdout).expect("parses");
        assert!(
            snap.nodes["child"].tags.is_empty(),
            "expected local tags only, got {:?}",
            snap.nodes["child"].tags
        );
    }

    #[test]
    fn parse_cx_list_handles_empty() {
        let snap = parse_cx_list(b"[]").expect("parses");
        assert!(snap.nodes.is_empty());
    }

    #[test]
    fn parse_cx_show_extracts_state_tags_shadowed() {
        // Mirrors actual `cx show <id> --json` shape — much richer
        // than cx list, but the parser only pulls the fields the
        // reconcile path cares about.
        let stdout = br#"{
            "id": "Ygdt",
            "state": "integrated",
            "tags": ["workflow:code-task"],
            "effective_tags": ["workflow:code-task"],
            "shadowed": false,
            "title": "...",
            "body": "long markdown body",
            "blockers": ["scxJ"],
            "blocking": [],
            "children": [],
            "created_at": "2026-04-09T16:04:18Z",
            "updated_at": "2026-04-10T14:14:35Z",
            "meta": {"_reason": "tiebreak"}
        }"#;
        let node = parse_cx_show(stdout).expect("parses");
        assert_eq!(node.node_id, "Ygdt");
        assert_eq!(node.state, "integrated");
        assert_eq!(node.tags, vec!["workflow:code-task"]);
        assert!(!node.shadowed);
    }

    #[test]
    fn parse_cx_show_returns_none_for_garbage() {
        assert!(parse_cx_show(b"not json").is_none());
        assert!(parse_cx_show(b"{}").is_none());
    }
}
