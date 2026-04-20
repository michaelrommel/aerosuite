//! HTTP server module for AeroFTP.
//!
//! This module provides a lightweight HTTP server that exposes Prometheus-compatible
//! metrics at the `/metrics` endpoint and a runtime configuration endpoint at `/config`.
//! The server:
//! * Listens on `[::]:9090` by default
//! * Supports graceful shutdown via broadcast channels
//! * Implements connection-level timeouts and error handling
//! * Uses `hyper` for high-performance HTTP/1.1 serving

mod server;

pub use server::start;

/// Handle for adjusting the active tracing filter at runtime.
///
/// Returned by `init_tracing()` in `main` and passed to the HTTP server so the
/// `/config` endpoint can update the log level without restarting the process.
/// Cloning the handle is cheap — it shares the underlying [`Arc`](std::sync::Arc).
pub type FilterHandle =
    tracing_subscriber::reload::Handle<tracing_subscriber::EnvFilter, tracing_subscriber::Registry>;
