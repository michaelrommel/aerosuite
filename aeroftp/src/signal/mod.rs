//! Signal handling module for AeroFTP.
//!
//! This module provides functionality to listen for and handle Unix signals:
//! * `SIGTERM` - Graceful shutdown request
//! * `SIGINT` - Interrupt signal (Ctrl+C)
//! * `SIGHUP` - Reload/restart signal
//!
//! The module implements a cooperative shutdown pattern where the application
//! can gracefully shut down or restart based on received signals.

mod watcher;

pub use watcher::{listen_for_signals, ExitSignal};
