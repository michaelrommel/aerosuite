//! WebSocket broadcaster: fan-out of [`DashboardUpdate`] JSON to all connected
//! aerotrack clients.
//!
//! `AppState` holds a `tokio::sync::broadcast::Sender<String>` that the delta
//! ticker writes to every ~3 s.  Each `WebSocket` client connection spawned by
//! [`ws_handler`] subscribes to a matching `Receiver` and forwards every JSON
//! string to the browser.

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::IntoResponse;
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

use crate::state::SharedState;

// ── axum WebSocket handler ────────────────────────────────────────────────

/// axum handler for `GET /ws`.
///
/// Upgrades the HTTP connection to a WebSocket and forwards every
/// `DashboardUpdate` JSON string from the broadcaster to the client.
pub async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<SharedState>,
) -> impl IntoResponse {
    // Subscribe before the upgrade so we don't miss a message that arrives
    // between the upgrade and the first `recv()`.
    let rx = state.read().await.ws_tx.subscribe();
    ws.on_upgrade(move |socket| handle_socket(socket, rx))
}

async fn handle_socket(mut socket: WebSocket, mut rx: broadcast::Receiver<String>) {
    info!("WebSocket client connected");
    loop {
        tokio::select! {
            // Forward the next broadcast message to the client.
            msg = rx.recv() => {
                match msg {
                    Ok(json) => {
                        if socket.send(Message::Text(json)).await.is_err() {
                            debug!("WebSocket client disconnected (send error)");
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!(
                            skipped = n,
                            "WebSocket client lagged — some messages skipped"
                        );
                        // Stay connected; the next recv() returns the oldest
                        // message still in the ring buffer.
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        // Broadcaster dropped — aerocoach is shutting down.
                        break;
                    }
                }
            }

            // Handle frames from the browser (ping / close / etc.).
            client_msg = socket.recv() => {
                match client_msg {
                    Some(Ok(Message::Close(_))) | None => {
                        debug!("WebSocket client closed connection");
                        break;
                    }
                    Some(Ok(Message::Ping(data))) => {
                        if socket.send(Message::Pong(data)).await.is_err() {
                            break;
                        }
                    }
                    _ => {} // Text/Binary frames from browser ignored.
                }
            }
        }
    }
    info!("WebSocket handler exiting");
}
