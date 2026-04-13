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

    // Collect node IDs touched by non-comment changes. Dedup via
    // BTreeSet — a single poll window can show N state transitions
    // and M tag edits for the same node, but we only want to fetch
    // its current state once. Comment events are activity, not
    // state, so they're emitted directly per-change below.
    let mut touched: std::collections::BTreeSet<String> = Default::default();
    let mut comment_events: Vec<DerivedCxEvent> = vec![];
    for entry in &entries {
        for change in &entry.changes {
            let action = change["action"].as_str().unwrap_or("");
            let node_id = match change["node_id"].as_str() {
                Some(s) => s,
                None => continue,
            };
            if action == "comment_added" {
                comment_events.push(DerivedCxEvent {
                    event_type: EventType::CxCommentAdded,
                    data: serde_json::to_value(CxCommentAddedData {
                        node_id: node_id.to_string(),
                        tag: change["tag"].as_str().map(String::from),
                        author: change["author"].as_str().map(String::from),
                    })
                    .unwrap(),
                });
            } else {
                touched.insert(node_id.to_string());
            }
        }
    }

    let mut events = vec![];
    for id in &touched {
        let snap = match fetch_node(repo_path, id) {
            Some(s) => s,
            None => continue, // node was deleted between log and show
        };
        if let Some(ev) = event_for_node_snapshot(&snap) {
            events.push(ev);
        }
    }
    events.extend(comment_events);

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

/// Apply a cx poll result via the caller's `append` function, returning the
/// cursor to advance to on full success. Short-circuits on the first append
/// failure and returns that error — callers MUST NOT advance the cursor on
/// error, because cx polling is cursor-based and skipped events are lost
/// forever. On the next tick the caller retries from the same cursor, which
/// may re-emit already-successful events; downstream handlers must tolerate
/// that (at-least-once delivery).
pub fn apply_poll_result<F>(result: CxPollResult, mut append: F) -> Result<Option<String>>
where
    F: FnMut(EventType, serde_json::Value) -> Result<()>,
{
    for ev in result.events {
        append(ev.event_type, ev.data)?;
    }
    Ok(result.latest_hash)
}

/// On first boot, snapshot the current cx state instead of replaying history.
/// Emits one event per non-latent node, via `event_for_node_snapshot`.
fn snapshot_current_ready_nodes(repo_path: &Path) -> Result<CxPollResult> {
    // Get current HEAD as the cursor
    let head_output = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo_path)
        .output()
        .context("git rev-parse HEAD")?;
    let head = String::from_utf8_lossy(&head_output.stdout).trim().to_string();

    let snap = match fetch_cx_state(repo_path) {
        Ok(s) => s,
        Err(_) => {
            // cx might not be initialized — that's fine, no events
            return Ok(CxPollResult {
                events: vec![],
                latest_hash: Some(head),
            });
        }
    };

    let mut events = vec![];
    for node in snap.nodes.values() {
        if let Some(ev) = event_for_node_snapshot(node) {
            events.push(ev);
        }
    }

    tracing::info!(event_count = events.len(), "cx snapshot: initial state");

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

/// Pure mapping from a cx node's current state to the ox event it
/// should produce. Returns `None` for states that aren't
/// trigger-relevant (latent, etc.). Source-of-truth conversion —
/// the reactive cx_poll_loop calls this once per touched node.
pub fn event_for_node_snapshot(snap: &CxNodeSnapshot) -> Option<DerivedCxEvent> {
    match snap.state.as_str() {
        "ready" => Some(DerivedCxEvent {
            event_type: EventType::CxTaskReady,
            data: serde_json::to_value(CxTaskReadyData {
                node_id: snap.node_id.clone(),
                tags: snap.tags.clone(),
                workflow: None,
                state: snap.state.clone(),
            })
            .unwrap(),
        }),
        "claimed" => Some(DerivedCxEvent {
            event_type: EventType::CxTaskClaimed,
            data: serde_json::to_value(CxTaskClaimedData {
                node_id: snap.node_id.clone(),
                part: None,
            })
            .unwrap(),
        }),
        "integrated" => Some(DerivedCxEvent {
            event_type: EventType::CxTaskIntegrated,
            data: serde_json::to_value(CxTaskIntegratedData {
                node_id: snap.node_id.clone(),
            })
            .unwrap(),
        }),
        _ => None,
    }
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

    // ── event_for_node_snapshot ────────────────────────────────────
    //
    // Pure mapping from cx truth → ox event. The reactive cx poll
    // loop calls this once per node touched by `cx log`, after
    // fetching the node's current snapshot via `cx show`. No state
    // diff interpretation — whatever cx says NOW is what we emit.

    fn snap(id: &str, state: &str, tags: &[&str]) -> CxNodeSnapshot {
        CxNodeSnapshot {
            node_id: id.into(),
            state: state.into(),
            tags: tags.iter().map(|s| s.to_string()).collect(),
            shadowed: false,
            shadow_reason: None,
            comment_count: 0,
        }
    }

    #[test]
    fn event_for_ready_snapshot() {
        let ev = event_for_node_snapshot(&snap("CcoT", "ready", &["workflow:code-task"]))
            .expect("ready → CxTaskReady");
        assert!(matches!(ev.event_type, EventType::CxTaskReady));
        let data: CxTaskReadyData = serde_json::from_value(ev.data).unwrap();
        assert_eq!(data.node_id, "CcoT");
        assert_eq!(data.tags, vec!["workflow:code-task"]);
        assert_eq!(data.state, "ready");
    }

    #[test]
    fn event_for_claimed_snapshot() {
        let ev = event_for_node_snapshot(&snap("aBcD", "claimed", &[]))
            .expect("claimed → CxTaskClaimed");
        assert!(matches!(ev.event_type, EventType::CxTaskClaimed));
    }

    #[test]
    fn event_for_integrated_snapshot() {
        let ev = event_for_node_snapshot(&snap("Ygdt", "integrated", &[]))
            .expect("integrated → CxTaskIntegrated");
        assert!(matches!(ev.event_type, EventType::CxTaskIntegrated));
    }

    #[test]
    fn event_for_latent_snapshot_emits_nothing() {
        assert!(event_for_node_snapshot(&snap("foo", "latent", &[])).is_none());
    }

    // ── apply_poll_result ──────────────────────────────────────────
    //
    // Regression: the cx poller previously advanced its cursor on every
    // tick, even when appending a derived event to the event log failed.
    // Because polling is a cursor-based diff, any skipped event was lost
    // permanently — the next tick started past it. apply_poll_result
    // returns the cursor hash ONLY if every event appended successfully;
    // callers must not advance the cursor on Err.

    fn ev(id: &str) -> DerivedCxEvent {
        DerivedCxEvent {
            event_type: EventType::CxTaskReady,
            data: serde_json::json!({ "node_id": id }),
        }
    }

    #[test]
    fn apply_poll_result_returns_cursor_when_all_appends_succeed() {
        let result = CxPollResult {
            events: vec![ev("A"), ev("B")],
            latest_hash: Some("deadbeef".into()),
        };
        let mut appended: Vec<String> = vec![];
        let cursor = apply_poll_result(result, |_t, d| {
            appended.push(d["node_id"].as_str().unwrap().into());
            Ok(())
        })
        .expect("all appends succeed");
        assert_eq!(cursor.as_deref(), Some("deadbeef"));
        assert_eq!(appended, vec!["A", "B"]);
    }

    #[test]
    fn apply_poll_result_returns_err_and_does_not_return_cursor_on_failure() {
        let result = CxPollResult {
            events: vec![ev("A"), ev("B"), ev("C")],
            latest_hash: Some("deadbeef".into()),
        };
        let mut appended: Vec<String> = vec![];
        let outcome = apply_poll_result(result, |_t, d| {
            let id: String = d["node_id"].as_str().unwrap().into();
            if id == "B" {
                anyhow::bail!("simulated db failure on B");
            }
            appended.push(id);
            Ok(())
        });
        assert!(outcome.is_err(), "failure must propagate so caller skips cursor advance");
        assert_eq!(appended, vec!["A"], "short-circuits on first failure");
    }

    #[test]
    fn apply_poll_result_with_no_events_returns_none_cursor() {
        // Empty-tick case: no events to append, no cursor to advance.
        // (poll_cx_log emits latest_hash=None when the cx log diff is empty.)
        let result = CxPollResult {
            events: vec![],
            latest_hash: None,
        };
        let cursor = apply_poll_result(result, |_, _| Ok(())).expect("no-op succeeds");
        assert_eq!(cursor, None);
    }
}
