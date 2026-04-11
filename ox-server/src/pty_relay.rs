//! PTY relay: bridges websocket connections between runners (PTY) and clients (ox-ctl attach).
//!
//! The runner connects to `/api/executions/{id}/steps/{step}/pty/runner` after spawning a PTY.
//! Clients connect to `/api/executions/{id}/steps/{step}/pty` to interact with the session.
//! The server bridges binary websocket frames between them.

use axum::{
    Router,
    extract::{Path, State, WebSocketUpgrade, ws::{Message, WebSocket}},
    response::IntoResponse,
    routing::get,
};
use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::sync::Mutex;
use tokio::sync::{broadcast, mpsc};

use crate::AppState;

/// Per-step PTY relay session.
pub struct PtyRelay {
    /// Runner sends PTY output here; clients subscribe via broadcast.
    pub output_tx: broadcast::Sender<Vec<u8>>,
    /// Clients send input here; runner reads from the receiver.
    pub input_tx: mpsc::Sender<Vec<u8>>,
    /// Runner takes this once on connect (Option so it can be taken).
    pub input_rx: Mutex<Option<mpsc::Receiver<Vec<u8>>>>,
    /// Notifies clients when the runner disconnects.
    pub shutdown: tokio::sync::watch::Sender<bool>,
}

/// Key for relay sessions: (execution_id, step_name).
type RelayKey = (String, String);

/// Thread-safe map of active PTY relay sessions.
pub type PtyRelays = Mutex<HashMap<RelayKey, std::sync::Arc<PtyRelay>>>;

pub fn new_relays() -> PtyRelays {
    Mutex::new(HashMap::new())
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/api/executions/{id}/steps/{step}/pty/runner",
            get(runner_ws),
        )
        .route(
            "/api/executions/{id}/steps/{step}/pty",
            get(client_ws),
        )
}

#[derive(serde::Deserialize)]
struct PtyPathParams {
    id: String,
    step: String,
}

/// Runner-side websocket: the runner connects here after spawning a PTY.
/// Binary frames from runner = PTY output. Binary frames to runner = PTY input.
async fn runner_ws(
    State(state): State<AppState>,
    Path(params): Path<PtyPathParams>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_runner_ws(state, params.id, params.step, socket))
}

async fn handle_runner_ws(
    state: AppState,
    exec_id: String,
    step: String,
    socket: WebSocket,
) {
    let key = (exec_id.clone(), step.clone());

    // Create relay session
    let (output_tx, _) = broadcast::channel::<Vec<u8>>(256);
    let (input_tx, input_rx) = mpsc::channel::<Vec<u8>>(256);
    let (shutdown_tx, _) = tokio::sync::watch::channel(false);

    let relay = std::sync::Arc::new(PtyRelay {
        output_tx: output_tx.clone(),
        input_tx,
        input_rx: Mutex::new(Some(input_rx)),
        shutdown: shutdown_tx,
    });

    // Register relay
    {
        let mut relays = state.pty_relays.lock().unwrap();
        relays.insert(key.clone(), relay.clone());
    }

    tracing::info!(exec = %exec_id, step = %step, "PTY relay: runner connected");

    // Take input_rx (only one runner per session)
    let mut input_rx = relay
        .input_rx
        .lock()
        .unwrap()
        .take()
        .expect("input_rx already taken");

    let (mut ws_tx, mut ws_rx) = socket.split();

    // Runner → clients: forward PTY output from ws to broadcast
    let output_tx_clone = output_tx.clone();
    let mut runner_to_clients = tokio::spawn(async move {
        while let Some(Ok(msg)) = ws_rx.next().await {
            match msg {
                Message::Binary(data) => {
                    let _ = output_tx_clone.send(data.to_vec());
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
    });

    // Clients → runner: forward client input from mpsc to ws
    let mut clients_to_runner = tokio::spawn(async move {
        while let Some(data) = input_rx.recv().await {
            if ws_tx.send(Message::Binary(data.into())).await.is_err() {
                break;
            }
        }
    });

    // Wait for either direction to finish
    tokio::select! {
        _ = &mut runner_to_clients => {}
        _ = &mut clients_to_runner => {}
    }
    runner_to_clients.abort();
    clients_to_runner.abort();

    // Signal all connected clients that the runner is gone
    tracing::info!(exec = %exec_id, step = %step, "PTY relay: sending shutdown signal");
    let _ = relay.shutdown.send(true);

    // Cleanup
    {
        let mut relays = state.pty_relays.lock().unwrap();
        relays.remove(&key);
    }

    tracing::info!(exec = %exec_id, step = %step, "PTY relay: runner disconnected");
}

/// Client-side websocket: ox-ctl attach connects here.
/// Binary frames to client = PTY output. Binary frames from client = PTY input.
async fn client_ws(
    State(state): State<AppState>,
    Path(params): Path<PtyPathParams>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_client_ws(state, params.id, params.step, socket))
}

async fn handle_client_ws(
    state: AppState,
    exec_id: String,
    step: String,
    mut socket: WebSocket,
) {
    let key = (exec_id.clone(), step.clone());

    // Look up relay session
    let relay = {
        let relays = state.pty_relays.lock().unwrap();
        relays.get(&key).cloned()
    };

    let relay = match relay {
        Some(r) => r,
        None => {
            tracing::warn!(exec = %exec_id, step = %step, "PTY relay: no runner session found");
            let (mut tx, _) = socket.split();
            let _ = tx.send(Message::Close(None)).await;
            return;
        }
    };

    tracing::info!(exec = %exec_id, step = %step, "PTY relay: client connected");

    let mut output_rx = relay.output_tx.subscribe();
    let input_tx = relay.input_tx.clone();
    let mut shutdown_rx = relay.shutdown.subscribe();

    // Single-task loop: don't split the socket so close propagates cleanly
    loop {
        tokio::select! {
            result = output_rx.recv() => {
                match result {
                    Ok(data) => {
                        if socket.send(Message::Binary(data.into())).await.is_err() {
                            tracing::info!("PTY relay: client ws send failed, breaking");
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::debug!(skipped = n, "PTY relay: client lagged, skipping");
                        continue;
                    }
                    Err(e) => {
                        tracing::info!(err = %e, "PTY relay: client output_rx closed");
                        break;
                    }
                }
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Binary(data))) => {
                        if input_tx.send(data.to_vec()).await.is_err() {
                            tracing::info!("PTY relay: client input_tx send failed, breaking");
                            break;
                        }
                    }
                    Some(Ok(Message::Close(frame))) => {
                        tracing::info!(?frame, "PTY relay: client got Close frame");
                        break;
                    }
                    None => {
                        tracing::info!("PTY relay: client socket.recv() returned None");
                        break;
                    }
                    Some(Err(e)) => {
                        tracing::info!(err = %e, "PTY relay: client socket.recv() error");
                        break;
                    }
                    other => {
                        tracing::debug!(?other, "PTY relay: client got non-binary message");
                    }
                }
            }
            _ = shutdown_rx.changed() => {
                tracing::info!("PTY relay: client handler received shutdown");
                let _ = socket.send(Message::Text("__ox_pty_eof__".into())).await;
                let _ = socket.send(Message::Close(None)).await;
                break;
            }
        }
    }

    tracing::info!(exec = %exec_id, step = %step, "PTY relay: client disconnected");
}
