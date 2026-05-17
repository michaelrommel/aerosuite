//! FTP server module for AeroFTP.
//!
//! This module provides the core FTP server functionality, including:
//! * AWS S3-backed storage via opendal
//! * JSON file-based authentication
//! * Graceful shutdown handling
//! * Prometheus metrics integration

mod server;
mod tracer;
pub mod auth;
pub mod provider;
pub mod user;

pub use server::start_ftp;
