//! Axum HTTP server that exposes `/metrics` in Prometheus text format.

use axum::{Router, extract::State, routing::get};
use tokio::net::TcpListener;
use tracing::info;

use super::MetricsStore;

/// Bind to `0.0.0.0:<port>` and serve `/metrics` forever.
/// Call inside `tokio::spawn`.
pub async fn serve(store: MetricsStore, port: u16) {
    let app = Router::new()
        .route("/metrics", get(handler))
        .with_state(store);

    let listener = TcpListener::bind(("0.0.0.0", port))
        .await
        .unwrap_or_else(|e| panic!("Failed to bind metrics port {port}: {e}"));

    info!(port, "metrics server listening");

    axum::serve(listener, app)
        .await
        .unwrap_or_else(|e| panic!("Metrics server crashed: {e}"));
}

async fn handler(State(store): State<MetricsStore>) -> impl axum::response::IntoResponse {
    let state = store.read().await;
    let body  = super::exposition::format(&*state);
    (
        [("Content-Type", "text/plain; version=0.0.4; charset=utf-8")],
        body,
    )
}
