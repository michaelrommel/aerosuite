//! HTTP metrics server module for AeroFTP.
//!
//! This module provides a lightweight HTTP server that exposes Prometheus-compatible
//! metrics at the `/metrics` endpoint. The server:
//! * Listens on `[::]:9090` by default
//! * Supports graceful shutdown via broadcast channels
//! * Implements connection-level timeouts and error handling
//! * Uses `hyper` for high-performance HTTP/1.1 serving

mod server;

pub use server::start;
