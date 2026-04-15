//! Shell-out to the `cx` CLI. The watcher's only direct dependency
//! on cx lives here — every `Command::new("cx")` in the process is
//! in this file.
//!
//! Copied from `ox-server/src/cx.rs` with the ox-event-specific shape
//! removed. Slice 5 of the event-sources migration deletes the
//! ox-server copy; until then both trees parse cx output identically.

use anyhow::{Context, Result};
use ox_core::client::{CxNodeSnapshot, CxStateSnapshot};
use std::path::Path;

/// One parsed entry from `cx log --json`. Only the fields the watcher
/// touches are extracted.
#[derive(serde::Deserialize)]
pub struct CxLogEntry {
    pub changes: Vec<serde_json::Value>,
    pub hash: String,
    #[allow(dead_code)]
    pub date: String,
    #[allow(dead_code)]
    pub subject: String,
}

/// Touched-nodes summary derived from a `cx log --json --since` diff.
pub struct CxLogDiff {
    /// Node ids whose state or tags changed — each needs a `cx show`
    /// refresh to produce a source event.
    pub touched: Vec<String>,
    /// Comment-added events observed in the diff window. Comments are
    /// activity, not node state, so they surface directly without a
    /// round-trip.
    pub comments: Vec<CxCommentEntry>,
    /// SHA of the most recent commit in the diff window, or `None`
    /// when the window was empty.
    pub latest_hash: Option<String>,
}

/// A comment observed in the cx log diff. The stable idempotency
/// shape is `(node_id, tag, author, hash)` — same comment on the same
/// commit will dedup server-side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CxCommentEntry {
    pub node_id: String,
    pub tag: Option<String>,
    pub author: Option<String>,
    pub hash: String,
}

/// Run `cx log --json --since <sha>` and summarise the diff. Returns
/// the touched node ids and any comment-added events observed,
/// together with the latest commit SHA.
///
/// Callers fetch the current state of each touched node via
/// [`fetch_node`] to build `node.ready` / `node.claimed` / `node.done`
/// source events. Comment entries are emitted directly.
pub fn poll_cx_log(repo_path: &Path, since_sha: &str) -> Result<CxLogDiff> {
    let output = std::process::Command::new("cx")
        .args(["log", "--json", "--since", since_sha])
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
        return Ok(CxLogDiff {
            touched: vec![],
            comments: vec![],
            latest_hash: None,
        });
    }

    summarise_log_entries(entries)
}

/// Pure helper — reduces `cx log` entries into the touched set plus
/// comment events. Extracted so unit tests can exercise the diffing
/// without spawning `cx`.
pub fn summarise_log_entries(entries: Vec<CxLogEntry>) -> Result<CxLogDiff> {
    if entries.is_empty() {
        return Ok(CxLogDiff {
            touched: vec![],
            comments: vec![],
            latest_hash: None,
        });
    }

    // `cx log` returns newest-first — the first entry is the head of
    // the diff window.
    let latest_hash = Some(entries[0].hash.clone());

    let mut touched: std::collections::BTreeSet<String> = Default::default();
    let mut comments: Vec<CxCommentEntry> = vec![];
    for entry in &entries {
        for change in &entry.changes {
            let action = change["action"].as_str().unwrap_or("");
            let Some(node_id) = change["node_id"].as_str() else {
                continue;
            };
            if action == "comment_added" {
                comments.push(CxCommentEntry {
                    node_id: node_id.to_string(),
                    tag: change["tag"].as_str().map(String::from),
                    author: change["author"].as_str().map(String::from),
                    hash: entry.hash.clone(),
                });
            } else {
                touched.insert(node_id.to_string());
            }
        }
    }

    Ok(CxLogDiff {
        touched: touched.into_iter().collect(),
        comments,
        latest_hash,
    })
}

/// Snapshot the current cx state. Used on cold-start when the server
/// reports `cursor: null` — the watcher fetches `cx list --json` and
/// emits one source event per currently actionable node, rather than
/// replaying the entire cx log history.
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

/// Current git HEAD of the repo, used as the cursor after a cold-start
/// snapshot.
pub fn current_head(repo_path: &Path) -> Result<String> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo_path)
        .output()
        .context("git rev-parse HEAD")?;
    if !out.status.success() {
        anyhow::bail!("git rev-parse HEAD failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Pure parser for `cx list --json` output. Uses local `tags` (not
/// `effective_tags`) to match current trigger-evaluation semantics.
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

/// Pure parser for `cx show <id> --json` output.
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

/// Run `cx show <id> --json` for a single node. Returns `None` if the
/// node does not exist or cx exits non-zero (e.g. was deleted between
/// `cx log` and `cx show`).
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

    #[test]
    fn parse_cx_list_maps_state_and_tags_correctly() {
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
                "id": "stuk",
                "state": "claimed",
                "tags": ["workflow:code-task"],
                "effective_tags": ["workflow:code-task"],
                "shadowed": true,
                "title": "claimed and shadowed"
            }
        ]"#;

        let snap = parse_cx_list(stdout).expect("parses");
        assert_eq!(snap.nodes.len(), 2);
        assert_eq!(snap.nodes["CcoT"].state, "ready");
        assert!(snap.nodes["stuk"].shadowed);
    }

    #[test]
    fn parse_cx_show_extracts_state_tags_shadowed() {
        let stdout = br#"{
            "id": "Ygdt",
            "state": "integrated",
            "tags": ["workflow:code-task"],
            "shadowed": false
        }"#;
        let node = parse_cx_show(stdout).expect("parses");
        assert_eq!(node.state, "integrated");
    }

    #[test]
    fn summarise_log_entries_extracts_touched_and_comments() {
        // cx log returns newest-first; the first entry wins as latest_hash.
        let entries = vec![
            CxLogEntry {
                changes: vec![
                    serde_json::json!({"action": "state_changed", "node_id": "CcoT"}),
                    serde_json::json!({
                        "action": "comment_added",
                        "node_id": "Ygdt",
                        "tag": "review",
                        "author": "alice"
                    }),
                ],
                hash: "deadbeef".into(),
                date: "2026-04-15".into(),
                subject: "test".into(),
            },
            CxLogEntry {
                changes: vec![serde_json::json!({"action": "created", "node_id": "Ygdt"})],
                hash: "cafe1234".into(),
                date: "2026-04-14".into(),
                subject: "test".into(),
            },
        ];

        let diff = summarise_log_entries(entries).expect("summarises");
        assert_eq!(diff.latest_hash.as_deref(), Some("deadbeef"));
        assert_eq!(diff.touched, vec!["CcoT".to_string(), "Ygdt".to_string()]);
        assert_eq!(diff.comments.len(), 1);
        assert_eq!(diff.comments[0].node_id, "Ygdt");
        assert_eq!(diff.comments[0].tag.as_deref(), Some("review"));
        assert_eq!(diff.comments[0].author.as_deref(), Some("alice"));
        assert_eq!(diff.comments[0].hash, "deadbeef");
    }

    #[test]
    fn summarise_log_entries_empty_returns_none_cursor() {
        let diff = summarise_log_entries(vec![]).expect("empty ok");
        assert!(diff.touched.is_empty());
        assert!(diff.comments.is_empty());
        assert!(diff.latest_hash.is_none());
    }
}
