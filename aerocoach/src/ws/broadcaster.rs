//! WebSocket broadcaster: fan-out of [`DashboardUpdate`] JSON to all connected
//! aerotrack clients.
//!
//! Not yet implemented — placeholder for Phase B (WS session).

// TODO (Phase B – WS session):
//   - Broadcaster { tx: tokio::sync::broadcast::Sender<String> }
//   - Broadcaster::send_json(&DashboardUpdate)
//   - axum WebSocket upgrade handler that subscribes to the broadcast channel
//   - auto-reconnect / dead-client cleanup
