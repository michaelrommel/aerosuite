//! Delta engine: computes [`DashboardUpdate`] payloads for WebSocket broadcasts.
//!
//! Not yet implemented — placeholder for Phase B (delta/WS session).

// TODO (Phase B – delta/WS session):
//   - DeltaEngine::compute(&MetricsStore) → DashboardUpdate
//   - Track last-broadcast state to emit only changed agents and new transfers
//   - Serialise DashboardUpdate to JSON for the WebSocket broadcaster
