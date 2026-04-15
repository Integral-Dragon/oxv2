//! HTTP client for ox-server's watcher endpoints.
//!
//! The watcher talks to the server through three calls:
//!
//! - `GET /api/watchers/cx/cursor` — read the last committed cursor
//!   on startup. Returns `None` on cold start.
//! - `POST /api/events/ingest` — submit a batch with CAS semantics.
//!   On 409 the caller re-fetches the cursor and rebuilds the batch.
//! - `POST /api/events/ingest` with an empty `events` array — used
//!   for liveness pings when nothing new was observed.
//!
//! The client owns no durable state. The last committed cursor lives
//! in `WatcherState` (in `main.rs`) and is updated only after a
//! successful 200.

use anyhow::Result;
use ox_core::events::SourceEventData;
use serde::{Deserialize, Serialize};

/// Request body for `POST /api/events/ingest`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestBody {
    pub source: String,
    pub cursor_before: Option<String>,
    pub cursor_after: String,
    pub events: Vec<SourceEventData>,
}

/// Response body for a successful ingest.
#[derive(Debug, Clone, Deserialize)]
pub struct IngestResponseBody {
    pub appended: u32,
    pub deduped: u32,
}

/// Outcome of one ingest attempt. Conflict is non-retryable at the
/// batch level — the caller must re-GET the cursor and rebuild.
#[derive(Debug)]
pub enum IngestOutcome {
    Committed(IngestResponseBody),
    Conflict {
        expected: Option<String>,
        actual: Option<String>,
    },
}

/// Cursor row returned by `GET /api/watchers/{source}/cursor`.
#[derive(Debug, Clone, Deserialize)]
pub struct CursorResponse {
    pub cursor: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

/// Thin HTTP client. The source name is baked in — one instance per
/// watcher process.
pub struct WatcherClient {
    base_url: String,
    source: String,
    http: reqwest::Client,
}

impl WatcherClient {
    pub fn new(base_url: impl Into<String>, source: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            source: source.into(),
            http: reqwest::Client::new(),
        }
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// `GET /api/watchers/{source}/cursor`.
    pub async fn fetch_cursor(&self) -> Result<Option<String>> {
        let _ = &self.http;
        let _ = &self.base_url;
        let _ = &self.source;
        unimplemented!("slice 3: fetch_cursor")
    }

    /// `POST /api/events/ingest` with a single retry on 409. Returns
    /// the outcome from the final attempt. Network/5xx errors bubble
    /// up as `Err` — callers are expected to back off and retry with
    /// the same batch.
    pub async fn post_batch(&self, _body: &IngestBody) -> Result<IngestOutcome> {
        unimplemented!("slice 3: post_batch")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, extract::{Path, State}, http::StatusCode, routing::{get, post}};
    use std::net::SocketAddr;
    use std::sync::Arc;
    use tokio::net::TcpListener;
    use tokio::sync::Mutex;

    // ── Mock ox-server ────────────────────────────────────────────

    #[derive(Clone, Default)]
    struct MockState {
        /// Current stored cursor for "cx". `None` until set.
        cursor: Arc<Mutex<Option<String>>>,
        /// Every batch received, in order.
        received_batches: Arc<Mutex<Vec<IngestBody>>>,
        /// If set, responds with this status on the next ingest POST
        /// (then clears). Used to simulate a 409 then success.
        inject_status: Arc<Mutex<Option<u16>>>,
    }

    #[derive(Serialize)]
    struct CursorRow {
        cursor: Option<String>,
        updated_at: Option<String>,
    }

    async fn get_cursor(
        State(state): State<MockState>,
        Path(_source): Path<String>,
    ) -> Json<CursorRow> {
        let cursor = state.cursor.lock().await.clone();
        Json(CursorRow {
            cursor,
            updated_at: Some("2026-04-15T00:00:00Z".into()),
        })
    }

    async fn ingest_batch(
        State(state): State<MockState>,
        Json(body): Json<IngestBody>,
    ) -> (StatusCode, Json<serde_json::Value>) {
        // Optional injected failure path.
        if let Some(status) = state.inject_status.lock().await.take() {
            let code = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            let stored = state.cursor.lock().await.clone();
            if code == StatusCode::CONFLICT {
                return (
                    code,
                    Json(serde_json::json!({
                        "error": "cursor_before mismatch",
                        "expected": body.cursor_before,
                        "actual": stored,
                    })),
                );
            }
            return (code, Json(serde_json::json!({ "error": "injected" })));
        }

        // Normal CAS path.
        let mut stored = state.cursor.lock().await;
        if *stored != body.cursor_before {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": "cursor_before mismatch",
                    "expected": body.cursor_before,
                    "actual": *stored,
                })),
            );
        }

        let appended = body.events.len() as u32;
        *stored = Some(body.cursor_after.clone());
        state.received_batches.lock().await.push(body);

        (
            StatusCode::OK,
            Json(serde_json::json!({ "appended": appended, "deduped": 0 })),
        )
    }

    async fn start_mock() -> (SocketAddr, MockState) {
        let state = MockState::default();
        let app = Router::new()
            .route("/api/watchers/{source}/cursor", get(get_cursor))
            .route("/api/events/ingest", post(ingest_batch))
            .with_state(state.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (addr, state)
    }

    fn sample_event(key: &str) -> SourceEventData {
        SourceEventData {
            source: "cx".into(),
            kind: "node.ready".into(),
            subject_id: "Q6cY".into(),
            idempotency_key: key.into(),
            tags: vec![],
            data: serde_json::json!({}),
        }
    }

    #[tokio::test]
    async fn fetch_cursor_returns_none_on_cold_start() {
        let (addr, _state) = start_mock().await;
        let client = WatcherClient::new(format!("http://{addr}"), "cx");
        let cursor = client.fetch_cursor().await.expect("cursor fetch");
        assert_eq!(cursor, None);
    }

    #[tokio::test]
    async fn fetch_cursor_returns_stored_value_after_ingest() {
        let (addr, state) = start_mock().await;
        *state.cursor.lock().await = Some("sha-abc".into());
        let client = WatcherClient::new(format!("http://{addr}"), "cx");
        let cursor = client.fetch_cursor().await.unwrap();
        assert_eq!(cursor.as_deref(), Some("sha-abc"));
    }

    #[tokio::test]
    async fn post_batch_happy_path_commits_and_advances_cursor() {
        let (addr, state) = start_mock().await;
        let client = WatcherClient::new(format!("http://{addr}"), "cx");

        let body = IngestBody {
            source: "cx".into(),
            cursor_before: None,
            cursor_after: "sha-abc".into(),
            events: vec![sample_event("Q6cY:node.ready:sha-abc")],
        };

        let outcome = client.post_batch(&body).await.unwrap();
        match outcome {
            IngestOutcome::Committed(r) => assert_eq!(r.appended, 1),
            other => panic!("expected Committed, got {other:?}"),
        }

        assert_eq!(state.cursor.lock().await.as_deref(), Some("sha-abc"));
        assert_eq!(state.received_batches.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn post_batch_returns_conflict_on_cas_mismatch() {
        let (addr, state) = start_mock().await;
        // Seed the server with a cursor so the watcher's posting
        // `cursor_before: None` conflicts.
        *state.cursor.lock().await = Some("sha-realhead".into());

        let client = WatcherClient::new(format!("http://{addr}"), "cx");
        let body = IngestBody {
            source: "cx".into(),
            cursor_before: None,
            cursor_after: "sha-new".into(),
            events: vec![sample_event("k1")],
        };

        let outcome = client.post_batch(&body).await.unwrap();
        match outcome {
            IngestOutcome::Conflict { expected, actual } => {
                assert_eq!(expected, None);
                assert_eq!(actual.as_deref(), Some("sha-realhead"));
            }
            other => panic!("expected Conflict, got {other:?}"),
        }

        // Cursor unchanged, nothing recorded.
        assert_eq!(state.cursor.lock().await.as_deref(), Some("sha-realhead"));
        assert!(state.received_batches.lock().await.is_empty());
    }
}
