use axum::{
    Router,
    extract::{Query, State},
    response::sse::{Event, KeepAlive, Sse},
    routing::get,
};
use futures_util::stream::Stream;
use serde::Deserialize;
use std::convert::Infallible;

use crate::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/api/events/stream", get(sse_handler))
}

#[derive(Deserialize, Default)]
struct SseQuery {
    last_event_id: Option<u64>,
}

async fn sse_handler(
    State(state): State<AppState>,
    Query(query): Query<SseQuery>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let after_seq = query.last_event_id.unwrap_or(0);
    let mut rx = state.bus.subscribe();

    let stream = async_stream::stream! {
        // Replay events after the given seq
        if let Ok(replayed) = state.bus.replay_after(after_seq) {
            for envelope in replayed {
                let data = serde_json::to_string(&envelope).unwrap_or_default();
                let event_type = serde_json::to_value(&envelope.event_type)
                    .unwrap()
                    .as_str()
                    .unwrap_or("unknown")
                    .to_string();
                yield Ok(Event::default()
                    .id(envelope.seq.0.to_string())
                    .event(event_type)
                    .data(data));
            }
        }

        // Stream live events
        loop {
            match rx.recv().await {
                Ok(envelope) => {
                    let data = serde_json::to_string(&envelope).unwrap_or_default();
                    let event_type = serde_json::to_value(&envelope.event_type)
                        .unwrap()
                        .as_str()
                        .unwrap_or("unknown")
                        .to_string();
                    yield Ok(Event::default()
                        .id(envelope.seq.0.to_string())
                        .event(event_type)
                        .data(data));
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(n, "SSE subscriber lagged, events dropped");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    break;
                }
            }
        }
    };

    Sse::new(stream).keep_alive(KeepAlive::default())
}
