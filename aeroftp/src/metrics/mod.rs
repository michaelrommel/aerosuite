//! Prometheus metrics collection and export module for AeroFTP.
//!
//! This module provides functionality to gather and encode Prometheus-compatible
//! metrics in text format. It integrates with the `prometheus` crate to collect
//! various server metrics including:
//! * FTP connection counts and throughput
//! * Request latency statistics
//! * Error rates and other performance indicators
//!
//! The gathered metrics can be exposed via an HTTP endpoint for Prometheus scrapers.

mod prometheus;

pub use prometheus::gather;
