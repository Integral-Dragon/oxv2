//! Map cx facts onto `SourceEventData` envelopes. Pure — no I/O.
//!
//! This is where the watcher's editorial decisions live: which cx
//! states become source events, what the `kind` strings are, how the
//! idempotency key is built, and which nodes are filtered out
//! server-ward (shadowed / integrated spawners, etc.).
//!
//! Everything else in the watcher is plumbing — fetch state, POST
//! the envelope, retry on failure. The interesting logic is here.

use ox_core::client::CxNodeSnapshot;
use ox_core::events::SourceEventData;

use crate::cx::CxCommentEntry;

/// Watcher identifier. Stamped onto every event so triggers can
/// filter by `source = "cx"`.
pub const SOURCE: &str = "cx";

/// Kinds the watcher emits. Strings are stable — triggers match on
/// them verbatim.
pub mod kinds {
    pub const NODE_READY: &str = "node.ready";
    pub const NODE_CLAIMED: &str = "node.claimed";
    pub const NODE_DONE: &str = "node.done";
    pub const COMMENT_ADDED: &str = "comment.added";
}

/// Build a source event from a cx node snapshot observed during a
/// poll tick. The `cursor_hash` is the short SHA of the cx log entry
/// that triggered the fetch — it lands in the idempotency key so
/// two ticks that observe the same state transition dedup
/// server-side.
///
/// Returns `None` for:
/// - `latent` nodes (no event kind is defined for them),
/// - `ready` nodes that are shadowed (state suppression — the
///   server-side matcher does not know cx's lifecycle, so the
///   watcher filters here).
///
/// Non-`ready` shadowed states still emit — `node.claimed` /
/// `node.done` are observational facts that downstream workflows may
/// care about regardless of shadowing.
pub fn snapshot_to_event(
    snap: &CxNodeSnapshot,
    cursor_hash: &str,
) -> Option<SourceEventData> {
    let kind = match snap.state.as_str() {
        "ready" => {
            if snap.shadowed {
                return None;
            }
            kinds::NODE_READY
        }
        "claimed" => kinds::NODE_CLAIMED,
        "integrated" => kinds::NODE_DONE,
        _ => return None,
    };

    let short = short_sha(cursor_hash);
    let idempotency_key = format!("{}:{}:{}", snap.node_id, kind, short);

    let data = serde_json::json!({
        "node_id": snap.node_id,
        "state": snap.state,
        "tags": snap.tags,
        "shadowed": snap.shadowed,
    });

    Some(SourceEventData {
        source: SOURCE.to_string(),
        kind: kind.to_string(),
        subject_id: snap.node_id.clone(),
        idempotency_key,
        tags: snap.tags.clone(),
        data,
    })
}

/// Build a source event from a comment-added log entry. The
/// idempotency key folds in author, tag, and the commit hash so two
/// ticks observing the same commit produce identical keys.
pub fn comment_to_event(comment: &CxCommentEntry) -> SourceEventData {
    let short = short_sha(&comment.hash);
    let tag_slot = comment.tag.as_deref().unwrap_or("-");
    let author_slot = comment.author.as_deref().unwrap_or("-");
    let idempotency_key = format!(
        "{}:{}:{}:{}:{}",
        comment.node_id, kinds::COMMENT_ADDED, author_slot, tag_slot, short
    );

    let data = serde_json::json!({
        "node_id": comment.node_id,
        "tag": comment.tag,
        "author": comment.author,
        "hash": comment.hash,
    });

    // Comments carry the tag as the event-level tag so triggers can
    // match `workflow:review` etc. without reaching into `data`.
    let tags = comment.tag.clone().into_iter().collect();

    SourceEventData {
        source: SOURCE.to_string(),
        kind: kinds::COMMENT_ADDED.to_string(),
        subject_id: comment.node_id.clone(),
        idempotency_key,
        tags,
        data,
    }
}

fn short_sha(sha: &str) -> &str {
    if sha.len() >= 12 { &sha[..12] } else { sha }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(id: &str, state: &str, tags: &[&str], shadowed: bool) -> CxNodeSnapshot {
        CxNodeSnapshot {
            node_id: id.into(),
            state: state.into(),
            tags: tags.iter().map(|s| s.to_string()).collect(),
            shadowed,
            shadow_reason: None,
            comment_count: 0,
        }
    }

    #[test]
    fn ready_node_becomes_node_ready_event() {
        let s = snap("Q6cY", "ready", &["workflow:code-task"], false);
        let ev = snapshot_to_event(&s, "a1b2c3d4e5f6aabb").expect("ready → node.ready");
        assert_eq!(ev.source, "cx");
        assert_eq!(ev.kind, "node.ready");
        assert_eq!(ev.subject_id, "Q6cY");
        assert_eq!(ev.tags, vec!["workflow:code-task".to_string()]);
        assert_eq!(ev.idempotency_key, "Q6cY:node.ready:a1b2c3d4e5f6");
        assert_eq!(ev.data["state"], "ready");
        assert_eq!(ev.data["node_id"], "Q6cY");
    }

    #[test]
    fn claimed_node_becomes_node_claimed_event() {
        let s = snap("stuk", "claimed", &["workflow:code-task"], false);
        let ev = snapshot_to_event(&s, "deadbeef11224455").expect("claimed → node.claimed");
        assert_eq!(ev.kind, "node.claimed");
        assert_eq!(ev.idempotency_key, "stuk:node.claimed:deadbeef1122");
    }

    #[test]
    fn integrated_node_becomes_node_done_event() {
        let s = snap("Ygdt", "integrated", &[], false);
        let ev = snapshot_to_event(&s, "cafebabe99887766").expect("integrated → node.done");
        assert_eq!(ev.kind, "node.done");
        assert!(ev.tags.is_empty());
    }

    #[test]
    fn latent_node_emits_nothing() {
        let s = snap("zzz", "latent", &[], false);
        assert!(snapshot_to_event(&s, "abc").is_none());
    }

    /// State suppression: a shadowed, ready node MUST NOT emit
    /// `node.ready`. This is the filter that protects the server-side
    /// matcher from knowing cx's lifecycle. The Ygdt regression that
    /// motivated the in-herder suppression lives here now.
    #[test]
    fn shadowed_ready_node_is_filtered() {
        let s = snap("Ygdt", "ready", &["workflow:code-task"], true);
        assert!(
            snapshot_to_event(&s, "abc").is_none(),
            "shadowed ready nodes must not emit node.ready"
        );
    }

    /// Shadowed `integrated` / `claimed` states are still observable
    /// facts — only `node.ready` (the spawner event) is filtered on
    /// shadow.
    #[test]
    fn shadowed_integrated_node_still_emits_node_done() {
        let s = snap("Ygdt", "integrated", &[], true);
        let ev = snapshot_to_event(&s, "abc").expect("integrated is observational");
        assert_eq!(ev.kind, "node.done");
    }

    #[test]
    fn idempotency_key_is_stable_for_same_inputs() {
        let s = snap("Q6cY", "ready", &[], false);
        let a = snapshot_to_event(&s, "aaaaaaaaaaaabbbb").unwrap();
        let b = snapshot_to_event(&s, "aaaaaaaaaaaabbbb").unwrap();
        assert_eq!(a.idempotency_key, b.idempotency_key);
    }

    #[test]
    fn idempotency_key_differs_across_cursor_hashes() {
        let s = snap("Q6cY", "ready", &[], false);
        let a = snapshot_to_event(&s, "aaaaaaaaaaaabbbb").unwrap();
        let b = snapshot_to_event(&s, "cccccccccccc1111").unwrap();
        assert_ne!(a.idempotency_key, b.idempotency_key);
    }

    #[test]
    fn comment_becomes_comment_added_event() {
        let c = CxCommentEntry {
            node_id: "Q6cY".into(),
            tag: Some("review".into()),
            author: Some("alice".into()),
            hash: "deadbeef11224455".into(),
        };
        let ev = comment_to_event(&c);
        assert_eq!(ev.source, "cx");
        assert_eq!(ev.kind, "comment.added");
        assert_eq!(ev.subject_id, "Q6cY");
        assert_eq!(ev.tags, vec!["review".to_string()]);
        assert_eq!(ev.idempotency_key, "Q6cY:comment.added:alice:review:deadbeef1122");
        assert_eq!(ev.data["author"], "alice");
    }

    #[test]
    fn comment_with_no_tag_uses_dash_slots() {
        let c = CxCommentEntry {
            node_id: "Q6cY".into(),
            tag: None,
            author: None,
            hash: "feedfacefeedface".into(),
        };
        let ev = comment_to_event(&c);
        assert!(ev.tags.is_empty());
        assert_eq!(ev.idempotency_key, "Q6cY:comment.added:-:-:feedfacefeed");
    }
}
